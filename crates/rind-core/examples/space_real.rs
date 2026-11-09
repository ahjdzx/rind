//! 只读的真实机器 smoke：分析一个目录的空间占用，打印耗时、最大的子项和大文件，什么都不移动。
//! `cargo run --release -p rind-core --example space_real -- ~/work`

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rind_core::size::format_bytes;
use rind_core::space::{analyze, SpaceOptions};
use rind_core::{Env, ScanProgress, Size};

fn main() {
    let env = Env::detect();
    let root = std::env::args()
        .nth(1)
        .map(|a| env.expand(&a))
        .unwrap_or_else(|| env.home.clone());
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let started = Instant::now();
    let last_done = AtomicU64::new(0);
    let progress_calls = AtomicU64::new(0);
    let report = analyze(&env, &root, &SpaceOptions::default(), &|p| {
        progress_calls.fetch_add(1, Ordering::Relaxed);
        if let ScanProgress::Measuring { done, .. } = p {
            last_done.store(done as u64, Ordering::Relaxed);
        }
    });
    let elapsed = started.elapsed();

    let size = |s: Size| match s {
        Size::Known(n) => format_bytes(n),
        Size::Unknown => "unknown".into(),
    };
    let r = &report.root;
    println!(
        "{}: {} in {} files, {:.2?} (partial={}, incomplete={}, unreadable={}, progress calls={}, last done={})",
        env.display(&root),
        size(r.size),
        r.files,
        elapsed,
        r.partial,
        report.incomplete,
        report.unreadable.len(),
        progress_calls.load(Ordering::Relaxed),
        last_done.load(Ordering::Relaxed),
    );

    println!("\ntop children:");
    for c in r.children.iter().take(15) {
        println!(
            "  {:>9} {:>9} files  {:?}{}{}  {}",
            size(c.size),
            c.files,
            c.kind,
            if c.partial { " partial" } else { "" },
            c.hold_back
                .as_ref()
                .map(|h| format!(" HOLD {h:?}"))
                .unwrap_or_default(),
            env.display(&c.path),
        );
    }

    println!("\ntop large files:");
    for f in report.large_files.iter().take(15) {
        println!(
            "  {:>9}  {}{}",
            format_bytes(f.size),
            env.display(&f.path),
            f.hold_back
                .as_ref()
                .map(|h| format!("  HOLD {h:?}"))
                .unwrap_or_default(),
        );
    }

    let nodes = count(r);
    println!(
        "\n{} tree nodes, {} large files, {} removables",
        nodes,
        report.large_files.len(),
        report.removables().len()
    );
    let unreadable: Vec<PathBuf> = report.unreadable.iter().take(5).cloned().collect();
    if !unreadable.is_empty() {
        println!("first unreadable: {unreadable:?}");
    }
}

fn count(node: &rind_core::space::SpaceNode) -> usize {
    1 + node.children.iter().map(count).sum::<usize>()
}
