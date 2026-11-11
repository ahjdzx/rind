//! 把 core 的结果打印成人能读的文字，或 `--json`。

use std::collections::{BTreeSet, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;

use rind_core::cleanup::{CleanupCategory, CleanupReport};
use rind_core::launch::LaunchItem;
use rind_core::orphans::OrphanReport;
use rind_core::space::{NodeKind, SpaceNode, SpaceOptions, SpaceReport};
use rind_core::{
    Confidence, Env, HistoryRecord, InstalledApp, ProtectReason, RemovalOutcome, RestoreOutcome,
    ScanProgress, Selection, Size, UninstallPlan,
};
use rind_i18n::{
    describe_cleanup_category, describe_confidence, describe_evidence, describe_hold_back,
    describe_kind, describe_launch_domain, describe_launch_status, describe_protect,
    describe_record_kind, describe_refusal, explain_cleanup_category, fill, format_size, t, Lang,
};
use serde_json::json;

use crate::maintain::{Status, Tally};

pub struct Render {
    pub lang: Lang,
    pub json: bool,
    pub progress: bool,
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
        if !self.progress {
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
            ScanProgress::Measuring { done, total: 0 } => {
                let _ = write!(
                    err,
                    "\r\x1b[2K{}",
                    say(
                        self.lang,
                        &format!("已看过 {done} 个文件…"),
                        &format!("{done} files looked at…")
                    )
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
            println!(
                "  ✗ {}  —  {}",
                env.display(path),
                describe_refusal(self.lang, why)
            );
        }
        for (path, err) in &outcome.failed {
            println!(
                "  ! {}  —  {}",
                env.display(path),
                describe_refusal(self.lang, err)
            );
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
                "{}  {}  {:<6} {:<28} {:>3} {}  {}{restored}",
                short_id(&r.id),
                r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                describe_record_kind(self.lang, r.kind),
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

/// 已选项的已知大小之和。
fn selected_size<'a>(
    items: impl Iterator<Item = (&'a PathBuf, Size)>,
    selected: &HashSet<PathBuf>,
) -> u64 {
    items
        .filter(|(p, _)| selected.contains(*p))
        .filter_map(|(_, s)| s.known())
        .sum()
}

fn mark(selected: bool, selectable: bool) -> &'static str {
    if selected {
        "[x]"
    } else if selectable {
        "[ ]"
    } else {
        " ✗ "
    }
}

fn sorted(selected: &HashSet<PathBuf>) -> BTreeSet<&PathBuf> {
    selected.iter().collect()
}

/// 卸载之外的子命令的输出。
impl Render {
    fn tally_line(&self, tally: &Tally) -> String {
        let fmt = rind_core::size::format_bytes;
        let mut line = fill(
            &say(
                self.lang,
                "{n} 项，共 {size}；建议移走 {sn} 项，{ss}",
                "{n} items, {size}; {sn} suggested, {ss}",
            ),
            &[
                ("n", &tally.items.to_string()),
                ("size", &fmt(tally.size)),
                ("sn", &tally.suggested_items.to_string()),
                ("ss", &fmt(tally.suggested_size)),
            ],
        );
        if tally.unknown > 0 {
            line.push_str(&fill(
                &say(self.lang, "（{u} 项大小未知）", " ({u} of unknown size)"),
                &[("u", &tally.unknown.to_string())],
            ));
        }
        line
    }

    fn selected_line(&self, n: usize, size: u64) -> String {
        fill(
            t(self.lang, "summary.selected"),
            &[
                ("n", &n.to_string()),
                ("size", &rind_core::size::format_bytes(size)),
            ],
        )
    }

    pub fn status(&self, env: &Env, status: &Status) {
        if self.json {
            return self.emit(status);
        }
        let fmt = rind_core::size::format_bytes;
        match &status.disk {
            Some(d) => println!(
                "{}",
                fill(
                    &say(
                        self.lang,
                        "磁盘（{path} 所在的卷）：已用 {used} / 共 {total}，还剩 {free}",
                        "Disk ({path}): {used} used of {total}, {free} available",
                    ),
                    &[
                        ("path", &env.display(&d.path)),
                        ("used", &fmt(d.used)),
                        ("total", &fmt(d.total)),
                        ("free", &fmt(d.available)),
                    ]
                )
            ),
            None => println!(
                "{}",
                say(
                    self.lang,
                    "读不到磁盘容量。",
                    "Disk capacity is unavailable."
                )
            ),
        }
        println!(
            "{}",
            fill(
                &say(
                    self.lang,
                    "可卸载的 App：{n} 个",
                    "Apps you can uninstall: {n}"
                ),
                &[("n", &status.apps.to_string())]
            )
        );
        let Some(scan) = &status.scan else {
            return;
        };
        println!();
        println!(
            "{}",
            fill(
                &say(
                    self.lang,
                    "孤儿残留：{g} 组，{line}",
                    "Orphaned files: {g} groups, {line}",
                ),
                &[
                    ("g", &scan.orphan_groups.to_string()),
                    ("line", &self.tally_line(&scan.orphans)),
                ]
            )
        );
        println!(
            "{}",
            fill(
                &say(self.lang, "可清理：{line}", "Cleanable: {line}"),
                &[("line", &self.tally_line(&scan.cleanup))]
            )
        );
        for c in &scan.categories {
            println!(
                "  {:<14} {:>9}  {}",
                c.name,
                fmt(c.tally.size),
                fill(
                    &say(self.lang, "{n} 项", "{n} items"),
                    &[("n", &c.tally.items.to_string())]
                )
            );
        }
        println!();
        println!(
            "{}",
            fill(
                &say(
                    self.lang,
                    "按 Rind 的建议一共能释放 {size}（`rind orphans`、`rind clean` 看明细）。",
                    "Following Rind's suggestions frees {size} (`rind orphans` and `rind clean` show the details).",
                ),
                &[("size", &fmt(scan.reclaimable))]
            )
        );
    }

    pub fn orphans(&self, env: &Env, report: &OrphanReport, selected: &HashSet<PathBuf>) {
        let all = || report.groups.iter().flat_map(|g| g.items.iter());
        let total: u64 = report.groups.iter().map(|g| g.known_size()).sum();
        let chosen = selected_size(all().map(|m| (&m.path, m.size)), selected);
        if report.degraded {
            self.note_err(rind_i18n::describe_orphans_degraded(self.lang));
        }
        if self.json {
            let groups: Vec<serde_json::Value> = report
                .groups
                .iter()
                .map(|g| {
                    let items: Vec<serde_json::Value> = g
                        .items
                        .iter()
                        .map(|m| {
                            let mut v = serde_json::to_value(m).unwrap_or_default();
                            v["reason"] = describe_evidence(self.lang, &m.evidence).into();
                            v["held_back"] = m
                                .hold_back
                                .as_ref()
                                .map(|h| describe_hold_back(self.lang, h))
                                .into();
                            v["suggested"] = m.suggested().into();
                            v["selected"] = selected.contains(&m.path).into();
                            v
                        })
                        .collect();
                    json!({
                        "key": g.key,
                        "display_name": g.display_name,
                        "confidence": g.confidence(),
                        "size": g.known_size(),
                        "items": items,
                    })
                })
                .collect();
            return self.emit(&json!({
                "groups": groups,
                "degraded": report.degraded,
                "total": total,
                "selected": sorted(selected),
                "selected_size": chosen,
            }));
        }
        if report.groups.is_empty() {
            println!(
                "{}",
                say(self.lang, "没有找到孤儿残留。", "No orphaned files found.")
            );
            return;
        }
        for g in &report.groups {
            println!(
                "{}  ({})  {}  {}",
                g.display_name,
                g.key,
                g.confidence()
                    .map(|c| badge(self.lang, c))
                    .unwrap_or_default(),
                rind_core::size::format_bytes(g.known_size())
            );
            for m in &g.items {
                println!(
                    "{} {:>9}  {}",
                    mark(selected.contains(&m.path), m.selectable()),
                    format_size(self.lang, m.size),
                    env.display(&m.path)
                );
                println!(
                    "              {}  ·  {}",
                    describe_kind(self.lang, m.kind),
                    describe_evidence(self.lang, &m.evidence)
                );
                if let Some(hold) = &m.hold_back {
                    println!("              {}", describe_hold_back(self.lang, hold));
                }
            }
            println!();
        }
        let n = all().count();
        println!(
            "{}",
            fill(
                &say(
                    self.lang,
                    "共 {g} 组 {n} 项，{size}。",
                    "{g} groups, {n} items, {size}."
                ),
                &[
                    ("g", &report.groups.len().to_string()),
                    ("n", &n.to_string()),
                    ("size", &rind_core::size::format_bytes(total)),
                ]
            )
        );
        println!("{}", self.selected_line(selected.len(), chosen));
    }

    pub fn cleanup(&self, env: &Env, report: &CleanupReport, selected: &HashSet<PathBuf>) {
        let total: u64 = report.items.iter().filter_map(|i| i.size.known()).sum();
        let chosen = selected_size(report.items.iter().map(|i| (&i.path, i.size)), selected);
        if self.json {
            let categories: Vec<serde_json::Value> = CleanupCategory::ALL
                .iter()
                .filter_map(|&c| {
                    let items = report.in_category(c);
                    if items.is_empty() {
                        return None;
                    }
                    let items: Vec<serde_json::Value> = items
                        .into_iter()
                        .map(|i| {
                            let mut v = serde_json::to_value(i).unwrap_or_default();
                            v["held_back"] = i
                                .hold_back
                                .as_ref()
                                .map(|h| describe_hold_back(self.lang, h))
                                .into();
                            v["selected"] = selected.contains(&i.path).into();
                            v
                        })
                        .collect();
                    Some(json!({
                        "category": c,
                        "name": describe_cleanup_category(self.lang, c),
                        "explanation": explain_cleanup_category(self.lang, c),
                        "size": report.category_size(c),
                        "items": items,
                    }))
                })
                .collect();
            return self.emit(&json!({
                "categories": categories,
                "total": total,
                "suggested": report.suggested(),
                "selected": sorted(selected),
                "selected_size": chosen,
            }));
        }
        if report.items.is_empty() {
            println!(
                "{}",
                say(self.lang, "没有找到可清理的东西。", "Nothing to clean.")
            );
            return;
        }
        let now = chrono::Utc::now();
        for c in CleanupCategory::ALL {
            let items = report.in_category(c);
            if items.is_empty() {
                continue;
            }
            println!(
                "{}  {}  · {}",
                describe_cleanup_category(self.lang, c),
                rind_core::size::format_bytes(report.category_size(c)),
                fill(
                    &say(self.lang, "{n} 项", "{n} items"),
                    &[("n", &items.len().to_string())]
                )
            );
            println!("  {}", explain_cleanup_category(self.lang, c));
            for i in items {
                let age = match (c, i.modified) {
                    (CleanupCategory::DevArtifacts, Some(m)) => fill(
                        &say(self.lang, "  （{d} 天没动）", "  (untouched for {d} days)"),
                        &[("d", &(now - m).num_days().max(0).to_string())],
                    ),
                    _ => String::new(),
                };
                println!(
                    "{} {:>9}  {}  {}{age}",
                    mark(selected.contains(&i.path), i.selectable()),
                    format_size(self.lang, i.size),
                    i.label,
                    env.display(&i.path)
                );
                if let Some(hold) = &i.hold_back {
                    println!("              {}", describe_hold_back(self.lang, hold));
                }
            }
            println!();
        }
        println!(
            "{}",
            fill(
                &say(self.lang, "共 {n} 项，{size}。", "{n} items, {size}."),
                &[
                    ("n", &report.items.len().to_string()),
                    ("size", &rind_core::size::format_bytes(total)),
                ]
            )
        );
        println!("{}", self.selected_line(selected.len(), chosen));
    }

    pub fn space(&self, env: &Env, report: &SpaceReport, opts: &SpaceOptions) {
        if self.json {
            return self.emit(report);
        }
        let root = &report.root;
        println!(
            "{}  {}  · {}{}",
            env.display(&root.path),
            format_size(self.lang, root.size),
            self.files(root.files),
            self.partial(root.partial)
        );
        let base = root.size.known().filter(|n| *n > 0);
        self.space_children(root, base, 1);

        println!();
        let min = rind_core::size::format_bytes(opts.large_file_min);
        if report.large_files.is_empty() {
            println!(
                "{}",
                fill(
                    &say(
                        self.lang,
                        "没有 ≥ {min} 的大文件。",
                        "No files of {min} or more."
                    ),
                    &[("min", &min)]
                )
            );
        } else {
            println!(
                "{}",
                fill(
                    &say(
                        self.lang,
                        "大文件（≥ {min}）：",
                        "Large files ({min} or more):"
                    ),
                    &[("min", &min)]
                )
            );
            for f in &report.large_files {
                let date = f
                    .modified
                    .map(|m| {
                        m.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d")
                            .to_string()
                    })
                    .unwrap_or_else(|| "----------".to_string());
                println!(
                    "  {:>9}  {date}  {}",
                    rind_core::size::format_bytes(f.size),
                    env.display(&f.path)
                );
            }
        }

        if report.incomplete {
            self.note_err(&say(
                self.lang,
                "分析没有跑完（超时或被打断），上面的大小只是下限。",
                "The analysis did not finish (timed out or interrupted); the sizes above are lower bounds.",
            ));
        }
        if !report.unreadable.is_empty() {
            let shown: Vec<String> = report
                .unreadable
                .iter()
                .take(5)
                .map(|p| env.display(p))
                .collect();
            let more = if report.unreadable.len() > shown.len() {
                "…"
            } else {
                ""
            };
            self.note_err(&fill(
                &say(
                    self.lang,
                    "{n} 个目录读不了（没有权限），相关大小只是下限：{list}{more}",
                    "{n} folders could not be read (no permission), so related sizes are lower bounds: {list}{more}",
                ),
                &[
                    ("n", &report.unreadable.len().to_string()),
                    ("list", &shown.join(", ")),
                    ("more", more),
                ],
            ));
        }
    }

    fn space_children(&self, node: &SpaceNode, base: Option<u64>, depth: usize) {
        let indent = "  ".repeat(depth);
        for child in &node.children {
            let pct = match (child.size.known(), base) {
                (Some(n), Some(base)) => format!("{:.1}%", n as f64 * 100.0 / base as f64),
                _ => "-".to_string(),
            };
            let name = match child.kind {
                NodeKind::Other => say(self.lang, "（其它）", "(other)"),
                NodeKind::Dir => format!("{}/", child.name),
                NodeKind::File => child.name.clone(),
            };
            let files = match child.kind {
                NodeKind::File => String::new(),
                _ => format!("  {}", self.files(child.files)),
            };
            let held = child
                .hold_back
                .as_ref()
                .map(|h| format!("  · {}", describe_hold_back(self.lang, h)))
                .unwrap_or_default();
            println!(
                "{indent}{:>9}  {pct:>6}  {name}{files}{}{held}",
                format_size(self.lang, child.size),
                self.partial(child.partial)
            );
            self.space_children(child, base, depth + 1);
        }
    }

    fn files(&self, n: u64) -> String {
        fill(
            &say(self.lang, "{n} 个文件", "{n} files"),
            &[("n", &n.to_string())],
        )
    }

    fn partial(&self, partial: bool) -> String {
        if partial {
            say(self.lang, "  （不完整）", "  (partial)")
        } else {
            String::new()
        }
    }

    pub fn launch_items(&self, env: &Env, items: &[LaunchItem]) {
        if self.json {
            return self.emit(&items);
        }
        if items.is_empty() {
            println!("{}", say(self.lang, "没有启动项。", "No launch items."));
            return;
        }
        for i in items {
            let mut facts = vec![
                describe_launch_domain(self.lang, i.domain).to_string(),
                describe_launch_status(self.lang, i.status),
            ];
            if i.run_at_load {
                facts.push(say(self.lang, "登录时启动", "runs at load"));
            }
            if i.keep_alive {
                facts.push(say(self.lang, "常驻", "kept alive"));
            }
            println!(
                "{} {}",
                if i.orphan { "✗" } else { "·" },
                i.label.as_deref().unwrap_or("-")
            );
            println!("    {}", facts.join("  ·  "));
            println!("    {}", env.display(&i.plist));
            if let Some(program) = &i.program {
                let missing = if i.program_exists {
                    String::new()
                } else {
                    say(self.lang, "（不存在）", " (missing)")
                };
                println!(
                    "    {}{missing}",
                    fill(
                        &say(self.lang, "程序：{p}", "Program: {p}"),
                        &[("p", &env.display(program))]
                    )
                );
            }
            match (&i.owner, i.orphan) {
                (Some(owner), _) => println!(
                    "    {}",
                    fill(
                        &say(self.lang, "属于 {name}", "Belongs to {name}"),
                        &[("name", &owner.name)]
                    )
                ),
                (None, true) => println!(
                    "    {}",
                    say(
                        self.lang,
                        "孤儿：没有主人，程序也不在了",
                        "Orphan: no owner, and its program is gone"
                    )
                ),
                (None, false) => {}
            }
            if let Some(hold) = &i.hold_back {
                println!("    {}", describe_hold_back(self.lang, hold));
            }
        }
        println!();
        println!(
            "{}",
            fill(
                &say(
                    self.lang,
                    "共 {n} 项，其中孤儿 {o} 项。",
                    "{n} items, {o} orphaned."
                ),
                &[
                    ("n", &items.len().to_string()),
                    ("o", &items.iter().filter(|i| i.orphan).count().to_string()),
                ]
            )
        );
    }

    pub fn launch_toggled(&self, env: &Env, item: &LaunchItem, enabled: bool) {
        let label = item.label.as_deref().unwrap_or("-");
        if self.json {
            return self.emit(&json!({
                "label": label,
                "plist": item.plist,
                "enabled": enabled,
            }));
        }
        let text = if enabled {
            say(self.lang, "已启用 {label}。", "Enabled {label}.")
        } else {
            say(
                self.lang,
                "已停用 {label}，{plist} 留在原处；rind launch enable {label} 可恢复。",
                "Disabled {label}; {plist} stays where it is. rind launch enable {label} turns it back on.",
            )
        };
        println!(
            "{}",
            fill(
                &text,
                &[("label", label), ("plist", &env.display(&item.plist))]
            )
        );
    }
}
