//! 一个路径移走能释放多少。按实际占用块算，超时或读不了就是未知。

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::{ScanProgress, Size};

/// 单个路径，带时间预算。文件按 `st_blocks * 512`；目录递归，不跟符号链接；
/// 任何读取失败或超时 → `Size::Unknown`。同一个 inode 的多个硬链接只算一次。
pub fn measure(path: &Path, budget: Duration) -> Size {
    measure_with(path, budget, None)
}

/// 同 `measure`，另外每隔一段就看一眼 `cancel`，置位了就停下并返回 `Unknown`。
pub fn measure_with(path: &Path, budget: Duration, cancel: Option<&AtomicBool>) -> Size {
    let deadline = Instant::now() + budget;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Size::Unknown;
    };
    if !meta.is_dir() {
        return Size::Known(meta.blocks() * 512);
    }
    let mut total = meta.blocks() * 512;
    let mut seen_links: HashSet<(u64, u64)> = HashSet::new();
    for (index, entry) in WalkDir::new(path)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .enumerate()
    {
        if index % 128 == 0 {
            if Instant::now() > deadline {
                return Size::Unknown;
            }
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Size::Unknown;
            }
        }
        let Ok(entry) = entry else {
            return Size::Unknown;
        };
        let Ok(meta) = entry.metadata() else {
            return Size::Unknown;
        };
        if meta.is_file() && meta.nlink() > 1 && !seen_links.insert((meta.dev(), meta.ino())) {
            continue;
        }
        total += meta.blocks() * 512;
    }
    Size::Known(total)
}

/// 一批路径并行度量，顺序与输入一致；每完成一项回调一次 `ScanProgress::Measuring`。
pub fn measure_all(
    paths: &[PathBuf],
    budget_each: Duration,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Vec<Size> {
    measure_all_with(paths, budget_each, None, on_progress)
}

/// 同 `measure_all`，`cancel` 置位后剩下的项不再度量，都是 `Unknown`。
pub fn measure_all_with(
    paths: &[PathBuf],
    budget_each: Duration,
    cancel: Option<&AtomicBool>,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Vec<Size> {
    let total = paths.len();
    let done = AtomicUsize::new(0);
    paths
        .par_iter()
        .map(|path| {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Size::Unknown;
            }
            let size = measure_with(path, budget_each, cancel);
            let done = done.fetch_add(1, Ordering::Relaxed) + 1;
            on_progress(ScanProgress::Measuring { done, total });
            size
        })
        .collect()
}

/// `1.2 GB`、`340 KB`、`0 B`。十进制单位，和 Finder 一致。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn formats_decimal_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_234), "1.2 KB");
        assert_eq!(format_bytes(123_456), "123 KB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
        assert_eq!(format_bytes(12_345_678_901_234), "12.3 TB");
    }

    #[test]
    fn measures_files_and_folders_by_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, vec![1u8; 10_000]).unwrap();
        let Size::Known(file_size) = measure(&file, Duration::from_secs(5)) else {
            panic!("a readable file has a known size");
        };
        assert!(
            file_size >= 10_000,
            "blocks cover the contents: {file_size}"
        );

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.bin"), vec![2u8; 20_000]).unwrap();
        let Size::Known(dir_size) = measure(dir.path(), Duration::from_secs(5)) else {
            panic!("a readable folder has a known size");
        };
        assert!(dir_size >= 30_000, "folder covers both files: {dir_size}");

        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(
            matches!(measure(&empty, Duration::from_secs(5)), Size::Known(_)),
            "an empty folder is known, not unknown"
        );
    }

    #[test]
    fn hard_links_count_once_and_symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        std::fs::write(&a, vec![1u8; 100_000]).unwrap();
        std::fs::hard_link(&a, dir.path().join("b.bin")).unwrap();
        let Size::Known(size) = measure(dir.path(), Duration::from_secs(5)) else {
            panic!()
        };
        assert!(
            size < 150_000,
            "two names of one file are counted once: {size}"
        );

        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("big.bin"), vec![1u8; 1_000_000]).unwrap();
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("link")).unwrap();
        let Size::Known(size) = measure(linked.path(), Duration::from_secs(5)) else {
            panic!()
        };
        assert!(
            size < 100_000,
            "what a symlink points to is not counted: {size}"
        );
    }

    #[test]
    fn missing_or_unreadable_is_unknown_never_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            measure(&dir.path().join("nope"), Duration::from_secs(1)),
            Size::Unknown
        );

        // SAFETY: geteuid 总是成功。
        if unsafe { libc::geteuid() } == 0 {
            return; // root 什么都读得了，这个用例对它没有意义
        }
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("x"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = measure(&locked, Duration::from_secs(1));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, Size::Unknown);

        let nested = dir.path().join("outer");
        std::fs::create_dir_all(nested.join("inner")).unwrap();
        std::fs::write(nested.join("inner/x"), b"x").unwrap();
        std::fs::set_permissions(nested.join("inner"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let result = measure(&nested, Duration::from_secs(1));
        std::fs::set_permissions(nested.join("inner"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            result,
            Size::Unknown,
            "an unreadable folder inside makes the whole thing unknown"
        );
    }

    #[test]
    fn measure_all_keeps_order_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, vec![0u8; 5_000]).unwrap();
        std::fs::write(&b, vec![0u8; 50_000]).unwrap();
        let missing = dir.path().join("missing");
        let reported = std::sync::Mutex::new(Vec::new());
        let sizes = measure_all(&[a, missing, b], Duration::from_secs(5), &|p| {
            reported.lock().unwrap().push(p)
        });
        assert!(matches!(sizes[0], Size::Known(n) if (5_000..50_000).contains(&n)));
        assert_eq!(sizes[1], Size::Unknown);
        assert!(matches!(sizes[2], Size::Known(n) if n >= 50_000));
        let reported = reported.lock().unwrap();
        assert_eq!(reported.len(), 3);
        assert!(reported.contains(&ScanProgress::Measuring { done: 3, total: 3 }));
    }

    #[test]
    fn a_cancelled_measurement_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..300 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x").unwrap();
        }
        let cancel = AtomicBool::new(true);
        assert_eq!(
            measure_with(dir.path(), Duration::from_secs(5), Some(&cancel)),
            Size::Unknown
        );
        let sizes = measure_all_with(
            &[dir.path().to_path_buf()],
            Duration::from_secs(5),
            Some(&cancel),
            &|_| {},
        );
        assert_eq!(sizes, vec![Size::Unknown]);
        let calm = AtomicBool::new(false);
        assert!(matches!(
            measure_with(dir.path(), Duration::from_secs(5), Some(&calm)),
            Size::Known(_)
        ));
    }
}
