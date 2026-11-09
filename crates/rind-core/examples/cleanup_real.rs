//! 只读的真实机器 smoke：扫一遍缓存、日志、开发产物，按类别打印，什么都不移动。
//! `cargo run -p rind-core --release --example cleanup_real -- [--no-size] [--all]`
//! 默认每类只打印最大的 15 项，`--all` 全部打印。

use std::time::Instant;

use rind_core::cleanup::{scan, CleanupCategory, CleanupOptions};
use rind_core::size::format_bytes;
use rind_core::{apps, Env, Size};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_size = args.iter().any(|a| a == "--no-size");
    let all = args.iter().any(|a| a == "--all");
    let env = Env::detect();

    let started = Instant::now();
    let installed = apps::scan_apps(&env);
    println!("{} apps in {:.2?}", installed.len(), started.elapsed());

    let opts = CleanupOptions {
        measure: !no_size,
        ..CleanupOptions::default()
    };
    let started = Instant::now();
    let report = scan(&env, &installed, &opts, &|_| {});
    println!(
        "{} items in {:.2?}\n",
        report.items.len(),
        started.elapsed()
    );

    let mut grand = 0;
    let mut grand_suggested = 0;
    for category in CleanupCategory::ALL {
        let items = report.in_category(category);
        let total = report.category_size(category);
        let suggested: u64 = items
            .iter()
            .filter(|i| i.suggested)
            .filter_map(|i| i.size.known())
            .sum();
        let unknown = items.iter().filter(|i| i.size == Size::Unknown).count();
        grand += total;
        grand_suggested += suggested;
        println!(
            "== {category:?}: {} items, {} (suggested {}, {} unknown)",
            items.len(),
            format_bytes(total),
            format_bytes(suggested),
            unknown
        );
        let shown = if all { items.len() } else { 15 };
        for i in items.iter().take(shown) {
            let size = match i.size {
                Size::Known(n) => format_bytes(n),
                Size::Unknown => "?".to_string(),
            };
            let modified = i
                .modified
                .map(|m| m.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "-".to_string());
            let hold = i
                .hold_back
                .as_ref()
                .map(|h| format!(" [{h:?}]"))
                .unwrap_or_default();
            println!(
                "  {} {:>9}  {modified}  {}  {}{hold}",
                if i.suggested { "*" } else { " " },
                size,
                i.label,
                env.display(&i.path)
            );
        }
        if items.len() > shown {
            println!("  … {} more", items.len() - shown);
        }
        println!();
    }
    println!(
        "total {} (suggested {})",
        format_bytes(grand),
        format_bytes(grand_suggested)
    );
}
