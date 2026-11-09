//! 只读的真实机器 smoke：列出全部启动项和它们的状态，什么都不改（只跑 `launchctl list` / `print-disabled`）。
//! `cargo run -p rind-core --example launch_real`

use std::collections::BTreeMap;
use std::time::Instant;

use rind_core::launch::{self, LaunchStatus};
use rind_core::{apps, Env};

fn main() {
    let env = Env::detect();
    let started = Instant::now();
    let all = apps::scan_apps(&env);
    println!("{} apps in {:.2?}", all.len(), started.elapsed());

    let started = Instant::now();
    let items = launch::list(&env, &all);
    println!(
        "{} launch items in {:.2?}\n",
        items.len(),
        started.elapsed()
    );

    let mut by_domain: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    for item in &items {
        *by_domain.entry(format!("{:?}", item.domain)).or_default() += 1;
        let status = match item.status {
            LaunchStatus::Loaded { pid: Some(_) } => "running".to_string(),
            other => format!("{other:?}"),
        };
        *by_status.entry(status).or_default() += 1;
    }
    println!("by domain: {by_domain:?}");
    println!("by status: {by_status:?}");
    println!(
        "orphans: {}, owned: {}, manageable: {}\n",
        items.iter().filter(|i| i.orphan).count(),
        items.iter().filter(|i| i.owner.is_some()).count(),
        items.iter().filter(|i| i.manageable()).count(),
    );

    for item in &items {
        println!(
            "{}{:?} {} [{:?}] run_at_load={} keep_alive={} owner={} hold_back={:?}\n    {} -> {}{}",
            if item.orphan { "ORPHAN " } else { "" },
            item.domain,
            item.label.as_deref().unwrap_or("(unreadable)"),
            item.status,
            item.run_at_load,
            item.keep_alive,
            item.owner.as_ref().map_or("-", |o| o.name.as_str()),
            item.hold_back,
            env.display(&item.plist),
            item.program
                .as_ref()
                .map_or("(no program)".to_string(), |p| p.display().to_string()),
            if item.program.is_some() && !item.program_exists {
                " (missing)"
            } else {
                ""
            },
        );
    }
}
