//! 卸载之外的子命令：总览、孤儿、清理、空间分析、移任意文件、启动项。都是 core 的薄封装；
//! 移动一律走 `remove::trash_items`（启动项走 `launch::remove`），确认与 History 的规矩和卸载一样。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use rind_core::cleanup::{self, CleanupCategory, CleanupOptions};
use rind_core::history::Ledger;
use rind_core::launch::{self, LaunchDomain, LaunchItem};
use rind_core::orphans::{self, OrphanOptions};
use rind_core::remove::{self, Batch, Removable};
use rind_core::space::{self, SpaceOptions};
use rind_core::uninstall::ExecuteError;
use rind_core::{
    apps, disk, protect, Env, HoldBack, RecordKind, RemovalOutcome, ScanProgress, Size, Source,
};
use rind_i18n::{
    describe_cleanup_category, describe_execute_error, describe_hold_back, describe_launch_domain,
    fill, Lang,
};
use serde::Serialize;

use crate::render::{say, Render};
use crate::{Choose, Confirm, LaunchAction};

/// `rind status` 的结果。
#[derive(Serialize)]
pub struct Status {
    /// 家目录所在卷；读不到是 null。
    pub disk: Option<DiskStatus>,
    /// 可卸载的 App 数（不含 macOS 自带的）。
    pub apps: usize,
    /// 只有 `--scan` 时才有。
    pub scan: Option<ScanSummary>,
}

#[derive(Serialize)]
pub struct DiskStatus {
    pub path: PathBuf,
    pub total: u64,
    pub available: u64,
    pub used: u64,
}

#[derive(Serialize)]
pub struct ScanSummary {
    pub orphan_groups: usize,
    pub orphans: Tally,
    pub cleanup: Tally,
    pub categories: Vec<CategoryTally>,
    /// Rind 建议移走的（孤儿 + 清理，同一路径只算一次）合计能释放多少。
    pub reclaimable: u64,
}

/// 一批候选的合计。`size` 只算已知大小，`unknown` 是大小未知的项数。
#[derive(Serialize, Default)]
pub struct Tally {
    pub items: usize,
    pub size: u64,
    pub unknown: usize,
    pub suggested_items: usize,
    pub suggested_size: u64,
}

#[derive(Serialize)]
pub struct CategoryTally {
    pub category: CleanupCategory,
    pub name: String,
    #[serde(flatten)]
    pub tally: Tally,
}

fn tally(items: impl Iterator<Item = (Size, bool)>) -> Tally {
    let mut t = Tally::default();
    for (size, suggested) in items {
        t.items += 1;
        match size.known() {
            Some(n) => t.size += n,
            None => t.unknown += 1,
        }
        if suggested {
            t.suggested_items += 1;
            t.suggested_size += size.known().unwrap_or(0);
        }
    }
    t
}

/// `--category` 的取值：core 的 serde 名（`app_caches`…），也接受连字符。
pub fn parse_category(s: &str) -> std::result::Result<CleanupCategory, String> {
    let name = s.trim().to_ascii_lowercase().replace('-', "_");
    serde_json::from_value(serde_json::Value::String(name)).map_err(|_| {
        let names: Vec<String> = CleanupCategory::ALL
            .iter()
            .map(|c| category_name(*c))
            .collect();
        format!("unknown category: {s} (one of: {})", names.join(", "))
    })
}

pub fn category_name(category: CleanupCategory) -> String {
    serde_json::to_value(category)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// `500M`、`2G`、`1.5GB`、`4096`：十进制单位，和显示用的 `format_bytes` 一致。
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let upper = s.trim().to_ascii_uppercase();
    let number = upper.strip_suffix('B').unwrap_or(&upper);
    let (digits, scale) = match number.chars().last() {
        Some('K') => (&number[..number.len() - 1], 1e3),
        Some('M') => (&number[..number.len() - 1], 1e6),
        Some('G') => (&number[..number.len() - 1], 1e9),
        Some('T') => (&number[..number.len() - 1], 1e12),
        _ => (number, 1.0),
    };
    match digits.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => Ok((n * scale) as u64),
        _ => Err(format!("not a size: {s} (e.g. 500M, 2G)")),
    }
}

/// 用户给的路径：`~` 展开，相对路径按当前目录，去掉 `.`。不解析符号链接——那是 `protect` 的事。
fn absolute(env: &Env, raw: &str) -> Result<PathBuf> {
    let path = env.expand(raw);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path.components().collect())
}

/// `--json` 下不能停下来问，没给 `--yes` 又不是预演就直接报错，免得扫描半天后才失败。
fn require_yes(out: &Render, confirm: &Confirm) -> Result<()> {
    if out.json && !confirm.dry_run && !confirm.yes {
        bail!(
            "{}",
            say(
                out.lang,
                "--json 模式下要加 --yes 或 --dry-run。",
                "With --json, pass --yes or --dry-run."
            )
        );
    }
    Ok(())
}

/// 按 `--path` / `--all` / 默认建议，从候选里挑出要移的。
fn choose(
    env: &Env,
    lang: Lang,
    candidates: &[Removable],
    suggested: Vec<PathBuf>,
    choose: &Choose,
) -> Result<HashSet<PathBuf>> {
    if !choose.paths.is_empty() {
        let mut picked = HashSet::new();
        for raw in &choose.paths {
            let wanted = absolute(env, raw)?;
            let shown = env.display(&wanted);
            let Some(found) = candidates.iter().find(|c| c.path == wanted) else {
                bail!(
                    "{}",
                    fill(
                        &say(
                            lang,
                            "{path} 不在扫描结果里，只能选列出来的路径。",
                            "{path} is not in the scan results; only listed paths can be chosen."
                        ),
                        &[("path", &shown)]
                    )
                );
            };
            if let Some(hold) = found.hold_back.as_ref().filter(|h| !h.selectable()) {
                bail!(
                    "{}",
                    fill(
                        &say(
                            lang,
                            "{path} 不能移动：{why}",
                            "{path} cannot be moved: {why}"
                        ),
                        &[("path", &shown), ("why", &describe_hold_back(lang, hold))]
                    )
                );
            }
            picked.insert(found.path.clone());
        }
        return Ok(picked);
    }
    if choose.all {
        return Ok(candidates
            .iter()
            .filter(|c| c.hold_back.as_ref().is_none_or(HoldBack::selectable))
            .map(|c| c.path.clone())
            .collect());
    }
    Ok(suggested.into_iter().collect())
}

/// 预演或真移。`refused` 是调用方事先挡下的（带人话理由），并进结果里一起报。
fn move_selected(
    env: &Env,
    out: &Render,
    batch: &Batch,
    candidates: &[Removable],
    selected: &HashSet<PathBuf>,
    confirm: &Confirm,
    refused: Vec<(PathBuf, String)>,
) -> Result<ExitCode> {
    if selected.is_empty() {
        if out.json {
            out.outcome(
                env,
                &RemovalOutcome {
                    dry_run: confirm.dry_run,
                    refused,
                    ..Default::default()
                },
            );
        } else {
            out.note(rind_i18n::t(out.lang, "state.nothing_selected"));
        }
        return Ok(ExitCode::from(2));
    }
    if confirm.dry_run {
        let result = remove::trash_items(env, batch, candidates, selected, true, None);
        return finish(env, out, result, refused);
    }
    let freed = candidates
        .iter()
        .filter(|c| selected.contains(&c.path))
        .filter_map(|c| c.size.known())
        .sum();
    if !ask(out, confirm, selected.len(), freed)? {
        return Ok(ExitCode::from(2));
    }
    let ledger = Ledger::open(env)?;
    let result = remove::trash_items(env, batch, candidates, selected, false, Some(&ledger));
    finish(env, out, result, refused)
}

fn ask(out: &Render, confirm: &Confirm, count: usize, freed: u64) -> Result<bool> {
    if confirm.yes || crate::confirm(out, count, freed)? {
        return Ok(true);
    }
    out.note(&say(out.lang, "没有动任何东西。", "Nothing was moved."));
    Ok(false)
}

/// 报结果。有拒绝或失败的退出码 1；History 写一半的照样把已移的说清楚。
fn finish(
    env: &Env,
    out: &Render,
    result: std::result::Result<RemovalOutcome, ExecuteError>,
    mut refused: Vec<(PathBuf, String)>,
) -> Result<ExitCode> {
    match result {
        Ok(mut outcome) => {
            refused.append(&mut outcome.refused);
            outcome.refused = refused;
            out.outcome(env, &outcome);
            if !out.json && !outcome.dry_run && !outcome.moved.is_empty() {
                out.note_err(&say(
                    out.lang,
                    "东西在废纸篓里：清空废纸篓后才真正释放空间。",
                    "Everything went to the Trash: the space is only freed once you empty it.",
                ));
            }
            Ok(if outcome.failed.is_empty() && outcome.refused.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Err(ExecuteError::Ledger { error, partial }) => {
            out.partial_outcome(env, &partial, &error);
            Ok(ExitCode::from(1))
        }
        Err(err) => bail!("{}", describe_execute_error(out.lang, &err)),
    }
}

pub fn status(env: &Env, out: &Render, scan: bool) -> Result<ExitCode> {
    let all_apps = apps::scan_apps(env);
    let disk = disk::usage(&env.home).map(|u| DiskStatus {
        path: env.home.clone(),
        total: u.total,
        available: u.available,
        used: u.used(),
    });
    let summary = scan.then(|| {
        let progress = |p: ScanProgress| out.progress(&p);
        let rules = crate::load_rules(env);
        let orphans = orphans::scan(env, &all_apps, &rules, &OrphanOptions::default(), &progress);
        let cleanup = cleanup::scan(env, &all_apps, &CleanupOptions::default(), &progress);
        // 孤儿和缓存可能指向同一个路径，合计时只算一次。
        let mut reclaim: HashMap<&Path, u64> = HashMap::new();
        for m in orphans.groups.iter().flat_map(|g| &g.items) {
            if m.suggested() {
                reclaim.insert(&m.path, m.size.known().unwrap_or(0));
            }
        }
        for i in cleanup.items.iter().filter(|i| i.suggested) {
            reclaim.insert(&i.path, i.size.known().unwrap_or(0));
        }
        ScanSummary {
            orphan_groups: orphans.groups.len(),
            orphans: tally(
                orphans
                    .groups
                    .iter()
                    .flat_map(|g| &g.items)
                    .map(|m| (m.size, m.suggested())),
            ),
            cleanup: tally(cleanup.items.iter().map(|i| (i.size, i.suggested))),
            categories: CleanupCategory::ALL
                .iter()
                .map(|&c| CategoryTally {
                    category: c,
                    name: describe_cleanup_category(out.lang, c).to_string(),
                    tally: tally(
                        cleanup
                            .in_category(c)
                            .into_iter()
                            .map(|i| (i.size, i.suggested)),
                    ),
                })
                .filter(|c| c.tally.items > 0)
                .collect(),
            reclaimable: reclaim.values().sum(),
        }
    });
    out.status(
        env,
        &Status {
            disk,
            apps: all_apps.iter().filter(|a| !a.is_system).count(),
            scan: summary,
        },
    );
    if !scan && !out.json {
        out.note_err(&say(
            out.lang,
            "加 --scan 再看孤儿残留和可清理的空间。",
            "Add --scan to also count orphaned files and what can be cleaned.",
        ));
    }
    Ok(ExitCode::SUCCESS)
}

pub fn orphans(env: &Env, out: &Render, how: &Choose, confirm: &Confirm) -> Result<ExitCode> {
    let moving = how.trash || confirm.dry_run;
    if moving {
        require_yes(out, confirm)?;
    }
    let all_apps = apps::scan_apps(env);
    let rules = crate::load_rules(env);
    let progress = |p: ScanProgress| out.progress(&p);
    let report = orphans::scan(env, &all_apps, &rules, &OrphanOptions::default(), &progress);
    let candidates = report.removables();
    let selected = choose(env, out.lang, &candidates, report.suggested(), how)?;
    if !moving {
        out.orphans(env, &report, &selected);
        if !out.json && !selected.is_empty() {
            out.note_err(&say(
                out.lang,
                "加 --trash 把打 [x] 的移到废纸篓（--dry-run 先看看）。",
                "Add --trash to move the items marked [x] to the Trash (--dry-run to preview).",
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !out.json {
        out.orphans(env, &report, &selected);
    }
    let touched: Vec<_> = report
        .groups
        .iter()
        .filter(|g| g.items.iter().any(|m| selected.contains(&m.path)))
        .collect();
    let (title, bundle_id) = match touched.as_slice() {
        [one] => (
            one.display_name.clone(),
            one.key.contains('.').then(|| one.key.clone()),
        ),
        many => (
            fill(
                &say(
                    out.lang,
                    "{n} 个已删除 App 的残留",
                    "Leftovers of {n} deleted apps",
                ),
                &[("n", &many.len().to_string())],
            ),
            None,
        ),
    };
    let batch = Batch {
        kind: RecordKind::Orphans,
        title,
        bundle_id,
        source: Source::Cli,
    };
    move_selected(
        env,
        out,
        &batch,
        &candidates,
        &selected,
        confirm,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn clean(
    env: &Env,
    out: &Render,
    categories: Vec<CleanupCategory>,
    project_roots: &[String],
    min_age: Option<u32>,
    how: &Choose,
    confirm: &Confirm,
) -> Result<ExitCode> {
    let moving = how.trash || confirm.dry_run;
    if moving {
        require_yes(out, confirm)?;
    }
    let mut opts = CleanupOptions {
        categories,
        ..CleanupOptions::default()
    };
    for raw in project_roots {
        let path = absolute(env, raw)?;
        let real = std::fs::canonicalize(&path)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| {
                anyhow!(
                    "{}",
                    fill(
                        &say(out.lang, "{path} 不是一个目录。", "{path} is not a folder."),
                        &[("path", &env.display(&path))]
                    )
                )
            })?;
        opts.project_roots.push(real);
    }
    if let Some(days) = min_age {
        opts.artifact_min_age_days = days;
    }
    let all_apps = apps::scan_apps(env);
    let progress = |p: ScanProgress| out.progress(&p);
    let mut report = cleanup::scan(env, &all_apps, &opts, &progress);
    let mut candidates = report.removables();
    let selected = choose(env, out.lang, &candidates, report.suggested(), how)?;
    if !moving {
        out.cleanup(env, &report, &selected);
        if !out.json && !selected.is_empty() {
            out.note_err(&say(
                out.lang,
                "加 --trash 把打 [x] 的移到废纸篓（--dry-run 先看看）。",
                "Add --trash to move the items marked [x] to the Trash (--dry-run to preview).",
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !out.json {
        out.cleanup(env, &report, &selected);
    }
    let touched: HashSet<CleanupCategory> = report
        .items
        .iter()
        .filter(|i| selected.contains(&i.path))
        .map(|i| i.category)
        .collect();
    let title = match touched.iter().collect::<Vec<_>>().as_slice() {
        [one] => describe_cleanup_category(out.lang, **one).to_string(),
        _ => say(out.lang, "缓存与构建产物", "Caches and build artifacts"),
    };
    let batch = Batch {
        kind: RecordKind::Cleanup,
        title,
        bundle_id: None,
        source: Source::Cli,
    };
    // 扫描之后可能有程序刚打开：动手前再看一次谁在运行，新近在用的会被 `trash_items` 拒绝。
    cleanup::recheck_in_use(env, &mut report, &all_apps, None);
    candidates = report.removables();
    move_selected(
        env,
        out,
        &batch,
        &candidates,
        &selected,
        confirm,
        Vec::new(),
    )
}

pub fn analyze(
    env: &Env,
    out: &Render,
    path: Option<&str>,
    depth: usize,
    top: usize,
    large_min: Option<u64>,
) -> Result<ExitCode> {
    let root = match path {
        Some(raw) => absolute(env, raw)?,
        None => env.home.clone(),
    };
    // 只读，所以把符号链接（如 /tmp）解析成真实目录再看。
    let real = std::fs::canonicalize(&root)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                fill(
                    &say(
                        out.lang,
                        "{path} 不是一个能读的目录。",
                        "{path} is not a readable folder."
                    ),
                    &[("path", &env.display(&root))]
                )
            )
        })?;
    let opts = SpaceOptions {
        max_depth: depth,
        max_children: top.max(1),
        large_file_limit: top.max(1),
        // 默认 100 MB：和 `--large-min` 一样按十进制，显示出来正好是 "100 MB"。
        large_file_min: large_min.unwrap_or(100_000_000),
        ..SpaceOptions::default()
    };
    let progress = |p: ScanProgress| out.progress(&p);
    let report = space::analyze(env, &real, &opts, &progress);
    out.space(env, &report, &opts);
    Ok(ExitCode::SUCCESS)
}

pub fn trash(env: &Env, out: &Render, raw: &[String], confirm: &Confirm) -> Result<ExitCode> {
    require_yes(out, confirm)?;
    let mut candidates: Vec<Removable> = Vec::new();
    let mut refused: Vec<(PathBuf, String)> = Vec::new();
    for raw in raw {
        let path = absolute(env, raw)?;
        if candidates.iter().any(|c| c.path == path) || refused.iter().any(|(p, _)| p == &path) {
            continue;
        }
        if std::fs::symlink_metadata(&path).is_err() {
            bail!(
                "{}",
                fill(
                    &say(out.lang, "{path} 不存在。", "{path} does not exist."),
                    &[("path", &env.display(&path))]
                )
            );
        }
        let hold_back = match protect::check(env, &path) {
            Some(why) => Some(HoldBack::Protected { why }),
            None if protect::is_system_owned(env, &path) => Some(HoldBack::SystemOwned),
            None => None,
        };
        match hold_back {
            Some(hold) => refused.push((path, describe_hold_back(out.lang, &hold))),
            None => candidates.push(Removable {
                size: rind_core::size::measure(&path, Duration::from_secs(20)),
                path,
                hold_back: None,
            }),
        }
    }
    let refusals: Vec<String> = refused
        .iter()
        .map(|(p, why)| format!("{}（{why}）", env.display(p)))
        .collect();
    if candidates.is_empty() {
        bail!(
            "{}",
            fill(
                &say(
                    out.lang,
                    "没有能移动的，Rind 不动这些：{list}",
                    "Nothing can be moved; Rind leaves these alone: {list}"
                ),
                &[("list", &refusals.join("; "))]
            )
        );
    }
    if !out.json {
        for line in &refusals {
            out.note_err(&fill(
                &say(out.lang, "✗ 不动：{item}", "✗ Left alone: {item}"),
                &[("item", line)],
            ));
        }
    }
    let title = match candidates.as_slice() {
        [one] => one
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| env.display(&one.path)),
        many => fill(
            &say(out.lang, "{n} 个文件或文件夹", "{n} files and folders"),
            &[("n", &many.len().to_string())],
        ),
    };
    let batch = Batch {
        kind: RecordKind::Files,
        title,
        bundle_id: None,
        source: Source::Cli,
    };
    let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
    move_selected(env, out, &batch, &candidates, &selected, confirm, refused)
}

pub fn launch(env: &Env, out: &Render, action: LaunchAction) -> Result<ExitCode> {
    let all_apps = apps::scan_apps(env);
    let items = launch::list(env, &all_apps);
    match action {
        LaunchAction::List { orphans, user } => {
            let shown: Vec<LaunchItem> = items
                .into_iter()
                .filter(|i| !orphans || i.orphan)
                .filter(|i| !user || i.domain == LaunchDomain::UserAgent)
                .collect();
            out.launch_items(env, &shown);
            Ok(ExitCode::SUCCESS)
        }
        LaunchAction::Disable { label } => toggle(env, out, &items, &label, false),
        LaunchAction::Enable { label } => toggle(env, out, &items, &label, true),
        LaunchAction::Remove { labels, confirm } => {
            require_yes(out, &confirm)?;
            let mut chosen: Vec<LaunchItem> = Vec::new();
            for label in &labels {
                let item = find_launch(env, out.lang, &items, label)?;
                if !chosen.iter().any(|c| c.plist == item.plist) {
                    chosen.push(item.clone());
                }
            }
            if !out.json {
                out.launch_items(env, &chosen);
            }
            let selected: HashSet<PathBuf> = chosen.iter().map(|i| i.plist.clone()).collect();
            if confirm.dry_run {
                let result = launch::remove(env, &items, &selected, Source::Cli, true, None);
                return finish(env, out, result, Vec::new());
            }
            if !ask(out, &confirm, selected.len(), 0)? {
                return Ok(ExitCode::from(2));
            }
            let ledger = Ledger::open(env)?;
            let result = launch::remove(env, &items, &selected, Source::Cli, false, Some(&ledger));
            finish(env, out, result, Vec::new())
        }
    }
}

fn toggle(
    env: &Env,
    out: &Render,
    items: &[LaunchItem],
    label: &str,
    enabled: bool,
) -> Result<ExitCode> {
    let item = find_launch(env, out.lang, items, label)?;
    launch::set_enabled(env, item, enabled)?;
    out.launch_toggled(env, item, enabled);
    Ok(ExitCode::SUCCESS)
}

/// 按 label 精确找（不分大小写）。同名的多份里只有一份能管就用它；找不到、不能管都说清楚为什么。
fn find_launch<'a>(
    env: &Env,
    lang: Lang,
    items: &'a [LaunchItem],
    label: &str,
) -> Result<&'a LaunchItem> {
    let wanted = label.to_lowercase();
    let matches: Vec<&LaunchItem> = items
        .iter()
        .filter(|i| {
            i.label
                .as_deref()
                .is_some_and(|l| l.to_lowercase() == wanted)
        })
        .collect();
    let manageable: Vec<&LaunchItem> = matches.iter().copied().filter(|i| i.manageable()).collect();
    match (manageable.as_slice(), matches.first()) {
        ([one], _) => Ok(one),
        ([], None) => Err(anyhow!(
            "{}",
            fill(
                &say(
                    lang,
                    "没有 label 是 {label} 的启动项。`rind launch list` 列出全部。",
                    "No launch item has the label {label}. `rind launch list` shows them all."
                ),
                &[("label", label)]
            )
        )),
        ([], Some(first)) => {
            let why = match &first.hold_back {
                Some(hold) => describe_hold_back(lang, hold),
                None if first.domain != LaunchDomain::UserAgent => fill(
                    &say(
                        lang,
                        "它属于「{domain}」，Rind 只管当前用户的启动项",
                        "it belongs to \"{domain}\"; Rind only manages this user's launch items",
                    ),
                    &[("domain", describe_launch_domain(lang, first.domain))],
                ),
                None => say(lang, "launchctl 认不出它", "launchctl cannot address it"),
            };
            Err(anyhow!(
                "{}",
                fill(
                    &say(
                        lang,
                        "{label}（{plist}）不能操作：{why}。",
                        "{label} ({plist}) cannot be changed: {why}."
                    ),
                    &[
                        ("label", label),
                        ("plist", &env.display(&first.plist)),
                        ("why", &why)
                    ]
                )
            ))
        }
        (many, _) => {
            let plists: Vec<String> = many.iter().map(|i| env.display(&i.plist)).collect();
            Err(anyhow!(
                "{}",
                fill(
                    &say(
                        lang,
                        "label 是 {label} 的启动项不止一个：{plists}。先手动处理多余的那份。",
                        "More than one launch item has the label {label}: {plists}. Sort out the extra copy by hand first."
                    ),
                    &[("label", label), ("plists", &plists.join("; "))]
                )
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_with_decimal_units() {
        assert_eq!(parse_size("500M"), Ok(500_000_000));
        assert_eq!(parse_size("2g"), Ok(2_000_000_000));
        assert_eq!(parse_size("1.5GB"), Ok(1_500_000_000));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("10K"), Ok(10_000));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1G").is_err());
        assert!(parse_size("B").is_err());
    }

    #[test]
    fn categories_parse_by_serde_name() {
        assert_eq!(parse_category("app_caches"), Ok(CleanupCategory::AppCaches));
        assert_eq!(
            parse_category("dev-artifacts"),
            Ok(CleanupCategory::DevArtifacts)
        );
        let err = parse_category("junk").unwrap_err();
        assert!(err.contains("package_caches"), "{err}");
    }
}
