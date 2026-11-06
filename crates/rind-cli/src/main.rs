//! `rind`：Rind 的命令行。每个子命令都是 rind-core 的薄封装；移动前一定先问，`--dry-run` 只看不动。

mod render;

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Result};
use clap::{Args, Parser, Subcommand};
use rind_core::history::Ledger;
use rind_core::rules::RuleSet;
use rind_core::uninstall::{self, ExecuteError, PlanOptions};
use rind_core::{
    apps, protect, running, Env, InstalledApp, RestoreOutcome, ScanProgress, Selection, Source,
};
use rind_i18n::{fill, Lang};

use render::{say, Render};

#[derive(Parser)]
#[command(
    name = "rind",
    version,
    about = "卸载 Mac 上的 App，连同它留下的每个文件，并说明每个文件为什么属于它。"
)]
struct Cli {
    /// 语言：zh 或 en（默认看 RIND_LANG / LANG）。
    #[arg(long, global = true, value_parser = parse_lang)]
    lang: Option<Lang>,
    /// 以 JSON 输出，给脚本和 AI agent 用。
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出已安装的 App。
    Apps {
        /// 连 macOS 自带的也列出来。
        #[arg(long)]
        all: bool,
    },
    /// 看一个 App 留下了什么，以及每个文件为什么属于它。只看，不动。
    Leftovers {
        #[command(flatten)]
        target: Target,
    },
    /// 把一个 App 和它的残留移到废纸篓。
    Uninstall {
        #[command(flatten)]
        target: Target,
        /// 只说会做什么，不动任何东西。
        #[arg(long)]
        dry_run: bool,
        /// 不问，直接做。
        #[arg(short = 'y', long)]
        yes: bool,
        /// 连 Rind 没替你选的也一起（可能的、共用的、读不了的）。保护区里的仍然不动。
        #[arg(long)]
        all: bool,
        /// 只移残留，App 本体留下。App 在运行时照样要先退出。
        #[arg(long)]
        keep_app: bool,
        /// App 在运行就先让它退出；等 5 秒。
        #[arg(long)]
        quit: bool,
        /// App 不肯退出就强制退出。
        #[arg(long)]
        force_quit: bool,
    },
    /// Rind 移走过什么。
    History,
    /// 把一次移动放回原处。
    Restore {
        /// `rind history` 里的 id，可只给开头几位。
        id: String,
    },
    /// Rind 永远不动的地方。
    Protected,
}

#[derive(Args)]
struct Target {
    /// App 的名字、bundle id 或路径，不分大小写，可只给一部分。
    app: String,
    /// 不度量大小，更快。
    #[arg(long)]
    no_size: bool,
}

fn parse_lang(s: &str) -> std::result::Result<Lang, String> {
    match s.to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh_cn" => Ok(Lang::ZhCn),
        "en" => Ok(Lang::En),
        other => Err(format!("unknown language: {other} (zh or en)")),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let lang = cli.lang.unwrap_or_else(Lang::detect);
    let out = Render {
        lang,
        json: cli.json,
    };
    match run(cli.command, &out) {
        Ok(code) => code,
        Err(err) => {
            out.error(&format!("{err:#}"));
            ExitCode::from(1)
        }
    }
}

fn run(command: Command, out: &Render) -> Result<ExitCode> {
    let env = Env::detect();
    match command {
        Command::Apps { all } => {
            let mut list = apps::scan_apps(&env);
            if !all {
                list.retain(|a| !a.is_system);
            }
            out.apps(&env, &list);
            Ok(ExitCode::SUCCESS)
        }
        Command::Leftovers { target } => {
            let all_apps = apps::scan_apps(&env);
            let app = pick(&env, &all_apps, &target.app, out, out.json)?;
            let plan = make_plan(&env, &app, &all_apps, !target.no_size, out);
            out.plan(&env, &plan, &plan.suggested_selection());
            Ok(ExitCode::SUCCESS)
        }
        Command::Uninstall {
            target,
            dry_run,
            yes,
            all,
            keep_app,
            quit,
            force_quit,
        } => {
            let all_apps = apps::scan_apps(&env);
            let app = pick(&env, &all_apps, &target.app, out, yes || out.json)?;
            // 预演不动任何东西，App 在不在运行都能看。真要动，哪怕只动残留，App 也得先退出。
            if !dry_run {
                ensure_quit(&app, quit, force_quit, out)?;
            }
            let plan = make_plan(&env, &app, &all_apps, !target.no_size, out);
            let mut selection = if all {
                Selection {
                    include_app: plan.app_hold_back.is_none(),
                    paths: plan
                        .leftovers
                        .iter()
                        .filter(|m| m.selectable())
                        .map(|m| m.path.clone())
                        .collect(),
                }
            } else {
                plan.suggested_selection()
            };
            if keep_app {
                selection.include_app = false;
            }
            // JSON 模式下 stdout 只放一个文档：结果。计划用 `rind leftovers --json` 看。
            if !out.json {
                out.plan(&env, &plan, &selection);
            }
            if !selection.include_app && selection.paths.is_empty() {
                out.note(rind_i18n::t(out.lang, "state.nothing_selected"));
                return Ok(ExitCode::from(2));
            }
            if dry_run {
                let outcome = uninstall::execute(&env, &plan, &selection, Source::Cli, true, None)?;
                out.outcome(&env, &outcome);
                return Ok(ExitCode::SUCCESS);
            }
            if !yes
                && !confirm(
                    out,
                    selection.paths.len() + usize::from(selection.include_app),
                    plan.freed_by(&selection),
                )?
            {
                out.note(&say(out.lang, "没有动任何东西。", "Nothing was moved."));
                return Ok(ExitCode::from(2));
            }
            let ledger = Ledger::open(&env)?;
            match uninstall::execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)) {
                Ok(outcome) => {
                    out.outcome(&env, &outcome);
                    Ok(if outcome.failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
                }
                // 东西已经在废纸篓里了，只是 History 没记全：照样把结果说清楚，再说明记录的事。
                Err(ExecuteError::Ledger { error, partial }) => {
                    out.partial_outcome(&env, &partial, &error);
                    Ok(ExitCode::from(1))
                }
                Err(ExecuteError::AppRunning) => bail!("{}", rind_i18n::t(out.lang, "state.running")),
                Err(ExecuteError::LedgerRequired) => bail!(
                    "{}",
                    say(
                        out.lang,
                        "打不开 History，所以什么都没动：没有记录的东西不进废纸篓。",
                        "History cannot be opened, so nothing was moved: nothing goes to the Trash unrecorded."
                    )
                ),
                Err(ExecuteError::AppMoveFailed(e)) => bail!(
                    "{}",
                    say(
                        out.lang,
                        &format!("应用本体没能移动，残留一个都没动：{e}"),
                        &format!("The app could not be moved, so none of its leftovers were touched: {e}")
                    )
                ),
                Err(err) => Err(err.into()),
            }
        }
        Command::History => {
            let ledger = Ledger::open(&env)?;
            out.history(&env, &ledger.list()?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Restore { id } => {
            let ledger = Ledger::open(&env)?;
            let records = ledger.list()?;
            let matches: Vec<_> = records.iter().filter(|r| r.id.starts_with(&id)).collect();
            let record = match matches.as_slice() {
                [one] => *one,
                [] => bail!(
                    "{}",
                    fill(
                        &say(out.lang, "没有这条记录：{id}", "No such record: {id}"),
                        &[("id", &id)]
                    )
                ),
                _ => bail!(
                    "{}",
                    fill(
                        &say(
                            out.lang,
                            "不止一条记录以 {id} 开头，多给几位。",
                            "More than one record starts with {id}; give more of it."
                        ),
                        &[("id", &id)]
                    )
                ),
            };
            let outcome = restore_record(&ledger, &env, &record.id)?;
            out.restore(&env, &outcome);
            // 一项都没放回去（废纸篓里没了、原位被占……）：退出码说明没达成。
            Ok(
                if outcome.restored.is_empty() && !outcome.skipped.is_empty() {
                    ExitCode::from(2)
                } else {
                    ExitCode::SUCCESS
                },
            )
        }
        Command::Protected => {
            out.protected(&env, &protect::protected_roots(&env));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// App 在运行就按 `--quit` / `--force-quit` 让它退出；还在运行就报错。优雅退出失败不是终点，
/// 给了 `--force-quit` 就接着强制。
fn ensure_quit(app: &InstalledApp, quit: bool, force_quit: bool, out: &Render) -> Result<()> {
    if !running::is_running(app) {
        return Ok(());
    }
    if quit || force_quit {
        out.note(&fill(
            &say(out.lang, "正在退出 {name}…", "Quitting {name}…"),
            &[("name", &app.name)],
        ));
        // `quit` 自己会等 5 秒；没退掉就 Err。
        if let Err(graceful) = running::quit(app, false) {
            if force_quit {
                out.note(&fill(
                    &say(
                        out.lang,
                        "{name} 没有退出，强制退出…",
                        "{name} did not quit; forcing…",
                    ),
                    &[("name", &app.name)],
                ));
                if let Err(forced) = running::quit(app, true) {
                    out.note(&say(
                        out.lang,
                        &format!("强制退出也没成：{forced}"),
                        &format!("Force quit failed too: {forced}"),
                    ));
                }
            } else {
                out.note(&say(
                    out.lang,
                    &format!("没有退出：{graceful}"),
                    &format!("Did not quit: {graceful}"),
                ));
            }
        }
    }
    if running::is_running(app) {
        bail!(
            "{}",
            fill(
                &say(
                    out.lang,
                    "{name} 还在运行。先退出它，或者加 --quit / --force-quit。",
                    "{name} is still running. Quit it first, or pass --quit / --force-quit."
                ),
                &[("name", &app.name)]
            )
        );
    }
    Ok(())
}

/// 按名字、bundle id 或路径找一个可卸载的 App。系统 App 先排除再匹配，这样查询正好等于某个系统 App
/// 的名字时还能模糊命中别的。歧义时把路径也列出来，好区分两份同名拷贝。
/// 按名字 / bundle id / 路径找 App。`strict`（`--yes`、`--json` 这类没人看着的场合）只接受完全相等的命中：
/// "微信" 模糊对上 "企业微信" 这种事，必须有人看过计划再确认。
fn pick(
    env: &Env,
    all_apps: &[InstalledApp],
    query: &str,
    out: &Render,
    strict: bool,
) -> Result<InstalledApp> {
    let removable: Vec<InstalledApp> = all_apps.iter().filter(|a| !a.is_system).cloned().collect();
    let expanded = env.expand(query);
    let query = if query.starts_with('~') {
        expanded.to_string_lossy().into_owned()
    } else {
        query.to_string()
    };
    let found = apps::find_app(&removable, &query);
    let is_exact = |a: &InstalledApp| apps::is_exact_match(a, &query);
    match found.as_slice() {
        [one] if is_exact(one) => Ok((*one).clone()),
        [one] if strict => Err(anyhow!(
            "{}",
            fill(
                &say(
                    out.lang,
                    "没有叫 {q} 的 App；模糊对上了 {name} ({path})。不交互时请给出完整的名字、bundle id 或路径。",
                    "No app is called {q}; the closest is {name} ({path}). Without a prompt, give the full name, bundle id, or path."
                ),
                &[("q", &query), ("name", &one.name), ("path", &env.display(&one.path))]
            )
        )),
        [one] => {
            out.note_err(&fill(
                &say(out.lang, "没有叫 {q} 的 App，按模糊匹配选中了 {name} ({path})。", "No app is called {q}; picked {name} ({path}) by a loose match."),
                &[("q", &query), ("name", &one.name), ("path", &env.display(&one.path))],
            ));
            Ok((*one).clone())
        }
        [] => Err(anyhow!(
            "{}",
            fill(
                &say(
                    out.lang,
                    "没找到叫 {q} 的 App。`rind apps` 列出全部。",
                    "No app matches {q}. `rind apps` lists them all."
                ),
                &[("q", &query)]
            )
        )),
        many => {
            // 名字完全相等的优先，免得 "Code" 同时命中 VS Code 和 Xcode。
            let exact: Vec<&&InstalledApp> = many
                .iter()
                .filter(|a| apps::is_exact_match(a, &query))
                .collect();
            if let [one] = exact.as_slice() {
                return Ok((**one).clone());
            }
            let listed: Vec<String> = many
                .iter()
                .map(|a| format!("{} ({})", a.name, env.display(&a.path)))
                .collect();
            Err(anyhow!(
                "{}",
                fill(
                    &say(out.lang, "{q} 对上了不止一个：{apps}。给出完整的名字、bundle id 或路径。", "{q} matches more than one: {apps}. Give the full name, bundle id, or path."),
                    &[("q", &query), ("apps", &listed.join("; "))]
                )
            ))
        }
    }
}

fn make_plan(
    env: &Env,
    app: &InstalledApp,
    all_apps: &[InstalledApp],
    measure: bool,
    out: &Render,
) -> rind_core::UninstallPlan {
    let rules = load_rules(env);
    let opts = PlanOptions {
        measure,
        ..PlanOptions::default()
    };
    let progress = |p: ScanProgress| out.progress(&p);
    uninstall::plan(env, app, all_apps, &rules, &opts, &progress)
}

/// core 的规则库入口。cask 索引随规则一起读。
fn load_rules(env: &Env) -> RuleSet {
    RuleSet::with_casks(env)
}

/// core 的放回入口。
fn restore_record(ledger: &Ledger, env: &Env, id: &str) -> Result<RestoreOutcome> {
    ledger.restore(env, id)
}

fn confirm(out: &Render, count: usize, freed: u64) -> Result<bool> {
    if out.json {
        bail!(
            "{}",
            say(
                out.lang,
                "--json 模式下要加 --yes 或 --dry-run。",
                "With --json, pass --yes or --dry-run."
            )
        );
    }
    let size = rind_core::size::format_bytes(freed);
    print!(
        "{} [y/N] ",
        fill(
            rind_i18n::t(out.lang, "confirm.body"),
            &[("n", &count.to_string()), ("size", &size)]
        )
    );
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是"
    ))
}
