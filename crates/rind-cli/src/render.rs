//! 把 core 的结果打印成人能读的文字，或 `--json`。

use std::io::{self, Write};
use std::path::PathBuf;

use rind_core::{
    Confidence, Env, HistoryRecord, InstalledApp, ProtectReason, RemovalOutcome, RestoreOutcome,
    ScanProgress, Selection, Size, UninstallPlan,
};
use rind_i18n::{
    describe_confidence, describe_evidence, describe_hold_back, describe_kind, describe_protect,
    fill, format_size, t, Lang,
};

pub struct Render {
    pub lang: Lang,
    pub json: bool,
}

/// CLI 自己的零散文案：中文和英文各一句。
pub fn say(lang: Lang, zh: &str, en: &str) -> String {
    match lang {
        Lang::ZhCn => zh.to_string(),
        Lang::En => en.to_string(),
    }
}

/// 记录 id 的前几位，够人认就行。按字符截，id 里有非 ASCII 也不会切坏。
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

impl Render {
    fn emit<T: serde::Serialize>(&self, value: &T) {
        let mut out = io::stdout().lock();
        let _ = serde_json::to_writer_pretty(&mut out, value);
        let _ = writeln!(out);
    }

    pub fn error(&self, message: &str) {
        if self.json {
            self.emit(&serde_json::json!({ "error": message }));
        } else {
            eprintln!("rind: {message}");
        }
    }

    pub fn note(&self, message: &str) {
        if !self.json {
            println!("{message}");
        }
    }

    /// 给人看的提醒，走 stderr，不混进结果里。
    pub fn note_err(&self, message: &str) {
        eprintln!("{message}");
    }

    /// 扫描进度走 stderr，这样 stdout 永远只有结果。
    pub fn progress(&self, progress: &ScanProgress) {
        if self.json {
            return;
        }
        let mut err = io::stderr().lock();
        match progress {
            ScanProgress::Location(name) => {
                let _ = write!(
                    err,
                    "\r\x1b[2K{}",
                    fill(t(self.lang, "state.scanning"), &[("location", name)])
                );
            }
            ScanProgress::Measuring { done, total } => {
                let _ = write!(
                    err,
                    "\r\x1b[2K{}",
                    fill(
                        t(self.lang, "state.measuring"),
                        &[("done", &done.to_string()), ("total", &total.to_string())]
                    )
                );
            }
            ScanProgress::Finished => {
                let _ = write!(err, "\r\x1b[2K");
            }
        }
        let _ = err.flush();
    }

    pub fn apps(&self, env: &Env, apps: &[InstalledApp]) {
        if self.json {
            return self.emit(&apps);
        }
        for app in apps {
            let version = app.version.as_deref().unwrap_or("");
            let source = match &app.source {
                rind_core::AppSource::AppStore => "App Store".to_string(),
                rind_core::AppSource::Homebrew(token) => format!("Homebrew: {token}"),
                rind_core::AppSource::Other => String::new(),
            };
            println!(
                "{:<32} {:<14} {:<34} {}",
                app.name,
                version,
                app.bundle_id.as_deref().unwrap_or("-"),
                if source.is_empty() {
                    env.display(&app.path)
                } else {
                    format!("{}  ({source})", env.display(&app.path))
                }
            );
        }
        println!();
        println!(
            "{}",
            say(
                self.lang,
                &format!("共 {} 个 App。", apps.len()),
                &format!("{} apps.", apps.len())
            )
        );
    }

    pub fn plan(&self, env: &Env, plan: &UninstallPlan, selection: &Selection) {
        if self.json {
            return self.emit(&serde_json::json!({ "plan": plan, "selection": selection }));
        }
        let app = &plan.app;
        println!();
        println!(
            "{}  {}  {}",
            app.name,
            app.version.as_deref().unwrap_or(""),
            app.bundle_id.as_deref().unwrap_or("")
        );
        let app_mark = if selection.include_app { "[x]" } else { "[ ]" };
        println!(
            "{app_mark} {:>9}  {}",
            format_size(self.lang, app.size),
            env.display(&app.path)
        );
        if let Some(hold) = &plan.app_hold_back {
            println!("          {}", describe_hold_back(self.lang, hold));
        }
        if plan.running {
            println!("          {}", t(self.lang, "state.running"));
        }
        println!();
        if plan.leftovers.is_empty() {
            println!("{}", t(self.lang, "state.no_leftovers"));
        }
        for m in &plan.leftovers {
            let mark = if selection.paths.contains(&m.path) {
                "[x]"
            } else if m.selectable() {
                "[ ]"
            } else {
                " ✗ "
            };
            println!(
                "{mark} {:>9}  {:<8} {}",
                format_size(self.lang, m.size),
                badge(self.lang, m.evidence.confidence),
                env.display(&m.path)
            );
            println!(
                "              {}  ·  {}",
                describe_kind(self.lang, m.kind),
                describe_evidence(self.lang, &m.evidence)
            );
            // 共用时 hold_back 已经把名字列出来了，不重复。
            if !m.rivals.is_empty()
                && !matches!(m.hold_back, Some(rind_core::HoldBack::SharedWith { .. }))
            {
                let names: Vec<&str> = m.rivals.iter().map(|r| r.name.as_str()).collect();
                println!(
                    "              {}",
                    say(
                        self.lang,
                        &format!("也被这些 App 使用：{}", names.join("、")),
                        &format!("Also used by: {}", names.join(", "))
                    )
                );
            }
            if let Some(hold) = &m.hold_back {
                println!("              {}", describe_hold_back(self.lang, hold));
            }
        }
        println!();
        let n = selection.paths.len() + usize::from(selection.include_app);
        let freed = rind_core::size::format_bytes(plan.freed_by(selection));
        println!(
            "{}",
            fill(
                t(self.lang, "summary.selected"),
                &[("n", &n.to_string()), ("size", &freed)]
            )
        );
        let unknown = plan
            .leftovers
            .iter()
            .filter(|m| m.size == Size::Unknown)
            .count();
        if unknown > 0 {
            println!(
                "{}",
                say(
                    self.lang,
                    &format!("{unknown} 项的大小未知（读不了或太慢），释放量只算已知的。"),
                    &format!("{unknown} items have an unknown size (unreadable or too slow); the total counts only what is known.")
                )
            );
        }
    }

    pub fn outcome(&self, env: &Env, outcome: &RemovalOutcome) {
        if self.json {
            return self.emit(outcome);
        }
        self.outcome_lines(env, outcome);
        if let Some(id) = &outcome.record_id {
            let short = short_id(id);
            println!(
                "{}",
                say(
                    self.lang,
                    &format!("放回：rind restore {short}"),
                    &format!("Put back: rind restore {short}")
                )
            );
        }
    }

    /// History 写到一半失败：`outcome` 里的 `moved` 已经在废纸篓里了，要说清楚。
    pub fn partial_outcome(&self, env: &Env, outcome: &RemovalOutcome, error: &str) {
        if self.json {
            return self.emit(&serde_json::json!({ "error": error, "partial": outcome }));
        }
        self.outcome_lines(env, outcome);
        println!(
            "{}",
            say(
                self.lang,
                &format!("History 没有记全：{error}。上面打 ✓ 的项已经在废纸篓里，`rind restore` 可能放不回它们，需要时从废纸篓手动拖回。"),
                &format!("History is incomplete: {error}. The items marked ✓ above are already in the Trash; `rind restore` may not put them back, so drag them out of the Trash by hand if needed.")
            )
        );
    }

    fn outcome_lines(&self, env: &Env, outcome: &RemovalOutcome) {
        if outcome.dry_run {
            // 预演里 `moved` 是"将会移动"的清单，不是已经动了的。
            println!("{}", t(self.lang, "state.dry_run"));
            println!(
                "{}",
                say(
                    self.lang,
                    "真做的话会移到废纸篓：",
                    "A real run would move these to the Trash:"
                )
            );
            for (path, _) in &outcome.moved {
                println!("  → {}", env.display(path));
            }
        } else {
            for (path, trashed) in &outcome.moved {
                println!("  ✓ {}  →  {}", env.display(path), env.display(trashed));
            }
        }
        for (path, why) in &outcome.refused {
            println!("  ✗ {}  —  {why}", env.display(path));
        }
        for (path, err) in &outcome.failed {
            println!("  ! {}  —  {err}", env.display(path));
        }
        println!();
        let size = rind_core::size::format_bytes(outcome.freed);
        if outcome.dry_run {
            println!(
                "{}",
                say(
                    self.lang,
                    &format!("将释放 {size}。"),
                    &format!("Would free {size}.")
                )
            );
        } else {
            println!(
                "{}",
                fill(t(self.lang, "summary.freed"), &[("size", &size)])
            );
        }
    }

    pub fn history(&self, env: &Env, records: &[HistoryRecord]) {
        if self.json {
            return self.emit(&records);
        }
        if records.is_empty() {
            println!("{}", t(self.lang, "history.empty"));
            return;
        }
        for r in records {
            let freed: u64 = r.items.iter().filter_map(|i| i.size.known()).sum();
            let restored = if r.restored {
                format!("  ({})", t(self.lang, "history.restored"))
            } else {
                String::new()
            };
            println!(
                "{}  {}  {:<28} {:>3} {}  {}{restored}",
                short_id(&r.id),
                r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                r.app_name,
                r.items.len(),
                say(self.lang, "项", "items"),
                rind_core::size::format_bytes(freed)
            );
            for item in &r.items {
                let mark = if item.restored {
                    "↩"
                } else if item.pending {
                    "…"
                } else {
                    " "
                };
                println!("          {mark} {}", env.display(&item.original));
            }
        }
    }

    pub fn restore(&self, env: &Env, outcome: &RestoreOutcome) {
        if self.json {
            return self.emit(outcome);
        }
        for path in &outcome.restored {
            println!("  ✓ {}", env.display(path));
        }
        for (path, why) in &outcome.skipped {
            println!("  ✗ {}  —  {why}", env.display(path));
        }
        let done = fill(
            t(self.lang, "history.restore_done"),
            &[("n", &outcome.restored.len().to_string())],
        );
        if outcome.skipped.is_empty() {
            println!("{done}");
        } else {
            let skipped = fill(
                t(self.lang, "history.restore_skipped"),
                &[("n", &outcome.skipped.len().to_string())],
            );
            println!(
                "{}",
                say(
                    self.lang,
                    &format!("{done}，{skipped}。"),
                    &format!("{done}, {skipped}.")
                )
            );
        }
    }

    pub fn protected(&self, env: &Env, roots: &[(PathBuf, ProtectReason)]) {
        if self.json {
            let list: Vec<serde_json::Value> = roots
                .iter()
                .map(|(p, r)| serde_json::json!({ "path": p, "reason": r }))
                .collect();
            return self.emit(&list);
        }
        println!(
            "{}",
            say(
                self.lang,
                "这些地方 Rind 永远不动，任何选择都不能覆盖：",
                "Rind never touches these, and no selection can override it:"
            )
        );
        for (path, reason) in roots {
            println!(
                "  {:<52} {}",
                env.display(path),
                describe_protect(self.lang, *reason)
            );
        }
        println!();
        println!(
            "{}",
            say(
                self.lang,
                "另外还有：家目录、~/Library 及其一层子目录本身、桌面/文稿/下载等文件夹本身、/System /usr 等系统路径、带加密钱包扩展的浏览器 profile、照片和音乐库、Rind 自己。",
                "Also: your home folder, ~/Library and its top-level folders themselves, Desktop/Documents/Downloads themselves, system paths like /System and /usr, browser profiles holding a crypto wallet extension, photo and music libraries, and Rind itself."
            )
        );
    }
}

fn badge(lang: Lang, confidence: Confidence) -> String {
    describe_confidence(lang, confidence).to_string()
}
