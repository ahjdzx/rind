//! 只读的真实机器 smoke：列出 App，对一个 App 跑残留扫描，什么都不移动。
//! `cargo run -p rind-core --example scan_real -- WeChat`

use std::time::Instant;

use rind_core::leftovers::{scan, ScanOptions};
use rind_core::rules::RuleSet;
use rind_core::{apps, Env, Size};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_size = args.iter().any(|a| a == "--no-size");
    let query = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "WeChat".to_string());
    let env = Env::detect();

    let started = Instant::now();
    let all = apps::scan_apps(&env);
    let removable = all.iter().filter(|a| !a.is_system).count();
    println!(
        "{} apps ({} removable) in {:.2?}",
        all.len(),
        removable,
        started.elapsed()
    );
    let with_team = all.iter().filter(|a| a.team_id.is_some()).count();
    let with_groups = all.iter().filter(|a| !a.app_groups.is_empty()).count();
    println!("  with team id: {with_team}, with app groups: {with_groups}");

    let found = apps::find_app(&all, &query);
    let Some(app) = found.first() else {
        eprintln!("no app matches {query:?}");
        std::process::exit(1);
    };
    println!(
        "\n{} ({}) v{} team={} source={:?} groups={:?} embedded={}",
        app.name,
        app.bundle_id.as_deref().unwrap_or("-"),
        app.version.as_deref().unwrap_or("-"),
        app.team_id.as_deref().unwrap_or("-"),
        app.source,
        app.app_groups,
        app.embedded_ids.len()
    );

    let rules_started = Instant::now();
    let rules = RuleSet::with_casks(&env);
    println!(
        "rules + cask index loaded in {:.2?}",
        rules_started.elapsed()
    );
    let started = Instant::now();
    let opts = ScanOptions {
        measure: !no_size,
        ..ScanOptions::default()
    };
    let matches = scan(&env, app, &all, &rules, &opts, &|_| {});
    println!(
        "\n{} leftovers in {:.2?}:",
        matches.len(),
        started.elapsed()
    );
    let mut total = 0u64;
    for m in &matches {
        let size = match m.size {
            Size::Known(n) => {
                total += n;
                rind_core::size::format_bytes(n)
            }
            Size::Unknown => "unknown".into(),
        };
        let suggested = if m.suggested() { "✓" } else { " " };
        let rivals: Vec<&str> = m.rivals.iter().map(|r| r.name.as_str()).collect();
        println!(
            "{suggested} {:<8} {:<16} {:>9}  {:?}={}  {}{}{}",
            format!("{:?}", m.evidence.confidence).to_lowercase(),
            format!("{:?}", m.kind),
            size,
            m.evidence.kind,
            m.evidence.matched,
            env.display(&m.path),
            if rivals.is_empty() {
                String::new()
            } else {
                format!("  shared with {rivals:?}")
            },
            m.hold_back
                .as_ref()
                .map(|h| format!("  HOLD {h:?}"))
                .unwrap_or_default(),
        );
    }
    println!("\nknown total: {}", rind_core::size::format_bytes(total));
}
