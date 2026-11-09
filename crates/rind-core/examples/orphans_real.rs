//! 只读的真实机器 smoke：找孤儿并打印，什么都不移动。
//! `cargo run -p rind-core --example orphans_real -- [--no-size] [--all]`

use std::time::Instant;

use rind_core::orphans::{scan, OrphanOptions};
use rind_core::rules::RuleSet;
use rind_core::size::format_bytes;
use rind_core::{apps, Env, EvidenceKind, Size};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_size = args.iter().any(|a| a == "--no-size");
    let all = args.iter().any(|a| a == "--all");
    let env = Env::detect();

    let started = Instant::now();
    let apps = apps::scan_apps(&env);
    let rules = RuleSet::with_casks(&env);
    println!(
        "{} apps, {} rules, cask index: {} in {:.2?}",
        apps.len(),
        rules.rules.len(),
        rules.cask.as_ref().map_or(0, |c| c.zap_paths.len()),
        started.elapsed()
    );

    let started = Instant::now();
    let opts = OrphanOptions {
        measure: !no_size,
        ..OrphanOptions::default()
    };
    let report = scan(&env, &apps, &rules, &opts, &|_| {});
    let items: usize = report.groups.iter().map(|g| g.items.len()).sum();
    println!(
        "{} groups, {} items, {} suggested in {:.2?}\n",
        report.groups.len(),
        items,
        report.suggested().len(),
        started.elapsed()
    );

    let shown = if all { report.groups.len() } else { 20 };
    for group in report.groups.iter().take(shown) {
        println!(
            "{} [{}] {:?} {}",
            group.display_name,
            group.key,
            group.confidence(),
            format_bytes(group.known_size())
        );
        for m in &group.items {
            let signal = match &m.evidence.kind {
                EvidenceKind::Orphan { signal } => format!("{signal:?}"),
                other => format!("{other:?}"),
            };
            let size = match m.size {
                Size::Known(n) => format_bytes(n),
                Size::Unknown => "?".into(),
            };
            println!(
                "    {:<9} {:<14} {:<8} {}{}{}",
                format!("{:?}", m.evidence.confidence),
                signal,
                size,
                env.display(&m.path),
                if m.suggested() { "  [x]" } else { "" },
                m.hold_back
                    .as_ref()
                    .map(|h| format!("  hold_back={h:?}"))
                    .unwrap_or_default()
            );
        }
    }
}
