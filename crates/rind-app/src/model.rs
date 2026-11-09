//! 纯函数视图模型：导航、勾选、分组、剪枝、面包屑。不碰 GPUI，全部有单元测试。

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use rind_core::cleanup::{CleanupCategory, CleanupReport};
use rind_core::launch::{LaunchDomain, LaunchItem};
use rind_core::orphans::OrphanReport;
use rind_core::space::{NodeKind, SpaceNode, SpaceReport};
use rind_core::{ScanProgress, Size};

/// 侧栏里的每一页。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Overview,
    Apps,
    Orphans,
    Cleanup,
    Space,
    Launch,
    History,
    Settings,
}

impl Page {
    /// 「清理」组里的页，按侧栏顺序。
    pub const CLEAN: [Page; 5] = [
        Page::Apps,
        Page::Orphans,
        Page::Cleanup,
        Page::Space,
        Page::Launch,
    ];
    /// 侧栏底部的页。
    pub const FOOTER: [Page; 2] = [Page::History, Page::Settings];
}

/// 按名字找页（`RIND_PAGE` 开发辅助用）。
pub fn page_from_name(name: &str) -> Option<Page> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "overview" => Page::Overview,
        "apps" => Page::Apps,
        "orphans" => Page::Orphans,
        "cleanup" => Page::Cleanup,
        "space" => Page::Space,
        "launch" => Page::Launch,
        "history" => Page::History,
        "settings" => Page::Settings,
        _ => return None,
    })
}

/// 一组项目的勾选状态（只看能选的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupCheck {
    None,
    Some,
    All,
}

/// 一组 `(路径, 能不能选)` 里，选中了多少。没有能选的就是 `None`。
pub fn group_check<'a>(
    selection: &BTreeSet<PathBuf>,
    items: impl IntoIterator<Item = (&'a Path, bool)>,
) -> GroupCheck {
    let (mut total, mut picked) = (0usize, 0usize);
    for (path, selectable) in items {
        if !selectable {
            continue;
        }
        total += 1;
        if selection.contains(path) {
            picked += 1;
        }
    }
    match (total, picked) {
        (0, _) | (_, 0) => GroupCheck::None,
        (t, p) if t == p => GroupCheck::All,
        _ => GroupCheck::Some,
    }
}

/// 整组勾上或取消。不能选的项永远不进选择。
pub fn set_group<'a>(
    selection: &mut BTreeSet<PathBuf>,
    items: impl IntoIterator<Item = (&'a Path, bool)>,
    on: bool,
) {
    for (path, selectable) in items {
        if on && selectable {
            selection.insert(path.to_path_buf());
        } else if !on {
            selection.remove(path);
        }
    }
}

/// 勾一项。不能选的忽略。
pub fn set_one(selection: &mut BTreeSet<PathBuf>, path: &Path, selectable: bool, on: bool) {
    if on && selectable {
        selection.insert(path.to_path_buf());
    } else if !on {
        selection.remove(path);
    }
}

/// 选中部分的合计：项数、已知大小之和、大小未知的项数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub count: usize,
    pub bytes: u64,
    pub unknown: usize,
}

pub fn picked<'a>(
    selection: &BTreeSet<PathBuf>,
    items: impl IntoIterator<Item = (&'a Path, Size)>,
) -> Picked {
    let mut out = Picked::default();
    for (path, size) in items {
        if !selection.contains(path) {
            continue;
        }
        out.count += 1;
        match size {
            Size::Known(n) => out.bytes += n,
            Size::Unknown => out.unknown += 1,
        }
    }
    out
}

/// 选择里去掉已经不存在的路径（重新扫描、剪枝之后）。
pub fn retain_existing<'a>(
    selection: &mut BTreeSet<PathBuf>,
    items: impl IntoIterator<Item = &'a Path>,
) {
    let alive: HashSet<&Path> = items.into_iter().collect();
    selection.retain(|p| alive.contains(p.as_path()));
}

/// `path` 是不是已经被移走（它本身或它的某个上级在 `moved` 里）。
pub fn is_gone(path: &Path, moved: &HashSet<PathBuf>) -> bool {
    path.ancestors().any(|a| moved.contains(a))
}

/// 去掉已移走的孤儿；空组一起去掉。
pub fn prune_orphans(report: &OrphanReport, moved: &HashSet<PathBuf>) -> OrphanReport {
    let mut out = report.clone();
    for group in &mut out.groups {
        group.items.retain(|m| !is_gone(&m.path, moved));
    }
    out.groups.retain(|g| !g.items.is_empty());
    out
}

/// 去掉已移走的清理项。
pub fn prune_cleanup(report: &CleanupReport, moved: &HashSet<PathBuf>) -> CleanupReport {
    let mut out = report.clone();
    out.items.retain(|i| !is_gone(&i.path, moved));
    out
}

/// 去掉已移走的启动项。
pub fn prune_launch(items: &[LaunchItem], moved: &HashSet<PathBuf>) -> Vec<LaunchItem> {
    items
        .iter()
        .filter(|i| !is_gone(&i.plist, moved))
        .cloned()
        .collect()
}

/// 树里去掉已移走的节点，并从上级的大小、文件数里减掉。
pub fn prune_space(report: &SpaceReport, moved: &HashSet<PathBuf>) -> SpaceReport {
    fn walk(node: &mut SpaceNode, moved: &HashSet<PathBuf>) -> (u64, u64) {
        let mut removed_bytes = 0;
        let mut removed_files = 0;
        node.children.retain(|c| {
            if is_gone(&c.path, moved) {
                removed_bytes += c.size.known().unwrap_or(0);
                removed_files += c.files;
                false
            } else {
                true
            }
        });
        for child in &mut node.children {
            let (b, f) = walk(child, moved);
            removed_bytes += b;
            removed_files += f;
        }
        if let Size::Known(n) = node.size {
            node.size = Size::Known(n.saturating_sub(removed_bytes));
        }
        node.files = node.files.saturating_sub(removed_files);
        (removed_bytes, removed_files)
    }
    let mut out = report.clone();
    walk(&mut out.root, moved);
    out.large_files.retain(|f| !is_gone(&f.path, moved));
    out
}

/// 一类清理项：类别、项在 `report.items` 里的下标（按大小降序）、已知大小之和。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupSection {
    pub category: CleanupCategory,
    pub items: Vec<usize>,
    pub total: u64,
}

/// 按 `CleanupCategory::ALL` 的顺序分节，空的类别不出现。
pub fn cleanup_sections(report: &CleanupReport) -> Vec<CleanupSection> {
    CleanupCategory::ALL
        .iter()
        .filter_map(|&category| {
            let mut items: Vec<usize> = report
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.category == category)
                .map(|(ix, _)| ix)
                .collect();
            if items.is_empty() {
                return None;
            }
            items.sort_by_key(|&ix| std::cmp::Reverse(report.items[ix].size.known().unwrap_or(0)));
            let total = items
                .iter()
                .filter_map(|&ix| report.items[ix].size.known())
                .sum();
            Some(CleanupSection {
                category,
                items,
                total,
            })
        })
        .collect()
}

/// Rind 建议清理的部分一共多大。
pub fn cleanup_suggested_bytes(report: &CleanupReport) -> u64 {
    report
        .items
        .iter()
        .filter(|i| i.suggested)
        .filter_map(|i| i.size.known())
        .sum()
}

pub fn orphan_suggested_bytes(report: &OrphanReport) -> u64 {
    report
        .groups
        .iter()
        .flat_map(|g| g.items.iter())
        .filter(|m| m.suggested())
        .filter_map(|m| m.size.known())
        .sum()
}

pub fn orphan_total_bytes(report: &OrphanReport) -> u64 {
    report.groups.iter().map(|g| g.known_size()).sum()
}

/// 启动项的筛选。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchFilter {
    All,
    Orphans,
    User,
}

impl LaunchFilter {
    pub const ALL: [LaunchFilter; 3] =
        [LaunchFilter::All, LaunchFilter::Orphans, LaunchFilter::User];

    pub fn matches(self, item: &LaunchItem) -> bool {
        match self {
            LaunchFilter::All => true,
            LaunchFilter::Orphans => item.orphan,
            LaunchFilter::User => item.domain == LaunchDomain::UserAgent,
        }
    }
}

/// 启动项给人看的名字：label，没有就用 plist 文件名。
pub fn launch_name(item: &LaunchItem) -> String {
    item.label.clone().unwrap_or_else(|| {
        item.plist
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.plist.display().to_string())
    })
}

/// 面包屑：从分析根到当前目录的每一级（含两端）。`current` 不在 `root` 下时只有根。
pub fn breadcrumbs(root: &Path, current: &Path) -> Vec<PathBuf> {
    let Ok(rest) = current.strip_prefix(root) else {
        return vec![root.to_path_buf()];
    };
    let mut out = vec![root.to_path_buf()];
    let mut at = root.to_path_buf();
    for part in rest.components() {
        at.push(part);
        out.push(at.clone());
    }
    out
}

/// 这个目录节点是不是因为超过了分析深度而没有子项，需要再分析一次。
pub fn needs_drill(node: &SpaceNode) -> bool {
    node.kind == NodeKind::Dir && node.children.is_empty() && node.files > 0
}

/// 在主报告和下钻缓存里找一个节点，优先用已经展开的那份。
pub fn locate<'a>(
    reports: impl IntoIterator<Item = &'a SpaceReport>,
    path: &Path,
) -> Option<&'a SpaceNode> {
    let mut fallback = None;
    for report in reports {
        if let Some(node) = report.find(path) {
            if !needs_drill(node) {
                return Some(node);
            }
            fallback.get_or_insert(node);
        }
    }
    fallback
}

/// `part` 占 `whole` 的比例（0..=1）。未知按 0。
pub fn share(part: Size, whole: Size) -> f32 {
    match (part, whole) {
        (Size::Known(p), Size::Known(w)) if w > 0 => (p as f64 / w as f64).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    }
}

/// 进度能不能换成百分比（0..=100）。
pub fn progress_percent(progress: &ScanProgress) -> Option<f32> {
    match progress {
        ScanProgress::Measuring { done, total } if *total > 0 => {
            Some((*done as f32 / *total as f32 * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rind_core::cleanup::CleanupItem;
    use rind_core::launch::LaunchStatus;
    use rind_core::orphans::OrphanGroup;
    use rind_core::{
        Confidence, Evidence, EvidenceKind, HoldBack, LeftoverKind, LeftoverMatch, OrphanSignal,
    };

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn group_check_counts_only_selectable_items() {
        let mut sel = BTreeSet::new();
        let a = p("/a");
        let b = p("/b");
        let c = p("/c");
        let items = || {
            vec![
                (a.as_path(), true),
                (b.as_path(), true),
                (c.as_path(), false),
            ]
        };
        assert_eq!(group_check(&sel, items()), GroupCheck::None);
        sel.insert(a.clone());
        assert_eq!(group_check(&sel, items()), GroupCheck::Some);
        set_group(&mut sel, items(), true);
        assert_eq!(group_check(&sel, items()), GroupCheck::All);
        assert!(
            !sel.contains(&c),
            "held-back items never join the selection"
        );
        set_group(&mut sel, items(), false);
        assert!(sel.is_empty());
        assert_eq!(
            group_check(&sel, vec![(c.as_path(), false)]),
            GroupCheck::None
        );
    }

    #[test]
    fn set_one_refuses_unselectable() {
        let mut sel = BTreeSet::new();
        set_one(&mut sel, Path::new("/x"), false, true);
        assert!(sel.is_empty());
        set_one(&mut sel, Path::new("/x"), true, true);
        assert_eq!(sel.len(), 1);
        set_one(&mut sel, Path::new("/x"), false, false);
        assert!(sel.is_empty());
    }

    #[test]
    fn picked_sums_known_and_counts_unknown() {
        let sel: BTreeSet<PathBuf> = [p("/a"), p("/b")].into();
        let a = p("/a");
        let b = p("/b");
        let c = p("/c");
        let got = picked(
            &sel,
            vec![
                (a.as_path(), Size::Known(10)),
                (b.as_path(), Size::Unknown),
                (c.as_path(), Size::Known(99)),
            ],
        );
        assert_eq!(
            got,
            Picked {
                count: 2,
                bytes: 10,
                unknown: 1
            }
        );
    }

    #[test]
    fn retain_existing_drops_vanished_paths() {
        let mut sel: BTreeSet<PathBuf> = [p("/a"), p("/b")].into();
        let a = p("/a");
        retain_existing(&mut sel, vec![a.as_path()]);
        assert_eq!(sel.into_iter().collect::<Vec<_>>(), vec![a]);
    }

    fn orphan(path: &str, size: u64) -> LeftoverMatch {
        LeftoverMatch {
            path: p(path),
            kind: LeftoverKind::Cache,
            size: Size::Known(size),
            evidence: Evidence {
                kind: EvidenceKind::Orphan {
                    signal: OrphanSignal::BundleIdGone,
                },
                confidence: Confidence::Likely,
                matched: "x".into(),
            },
            rivals: vec![],
            hold_back: None,
        }
    }

    #[test]
    fn pruning_orphans_drops_moved_items_and_empty_groups() {
        let report = OrphanReport {
            groups: vec![
                OrphanGroup {
                    key: "a".into(),
                    display_name: "A".into(),
                    items: vec![orphan("/L/a1", 1), orphan("/L/a2", 2)],
                },
                OrphanGroup {
                    key: "b".into(),
                    display_name: "B".into(),
                    items: vec![orphan("/L/b1/inner", 3)],
                },
            ],
            degraded: false,
        };
        let moved: HashSet<PathBuf> = [p("/L/a1"), p("/L/b1")].into();
        let out = prune_orphans(&report, &moved);
        assert_eq!(out.groups.len(), 1);
        assert_eq!(out.groups[0].items.len(), 1);
        assert_eq!(orphan_total_bytes(&out), 2);
        assert_eq!(orphan_suggested_bytes(&report), 6);
    }

    fn citem(path: &str, category: CleanupCategory, size: Size, suggested: bool) -> CleanupItem {
        CleanupItem {
            path: p(path),
            category,
            label: path.into(),
            size,
            modified: None,
            hold_back: None,
            suggested,
        }
    }

    #[test]
    fn cleanup_sections_follow_category_order_and_sort_by_size() {
        let report = CleanupReport {
            items: vec![
                citem("/logs", CleanupCategory::Logs, Size::Known(5), true),
                citem("/c1", CleanupCategory::AppCaches, Size::Known(1), true),
                citem("/c2", CleanupCategory::AppCaches, Size::Known(9), false),
                citem("/c3", CleanupCategory::AppCaches, Size::Unknown, false),
            ],
        };
        let sections = cleanup_sections(&report);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].category, CleanupCategory::AppCaches);
        assert_eq!(sections[0].items, vec![2, 1, 3]);
        assert_eq!(sections[0].total, 10);
        assert_eq!(sections[1].category, CleanupCategory::Logs);
        assert_eq!(cleanup_suggested_bytes(&report), 6);
        let moved: HashSet<PathBuf> = [p("/c2")].into();
        assert_eq!(prune_cleanup(&report, &moved).items.len(), 3);
    }

    fn node(
        path: &str,
        kind: NodeKind,
        size: u64,
        files: u64,
        children: Vec<SpaceNode>,
    ) -> SpaceNode {
        SpaceNode {
            path: p(path),
            name: Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            kind,
            size: Size::Known(size),
            files,
            partial: false,
            modified: None,
            hold_back: None,
            children,
        }
    }

    fn tree() -> SpaceReport {
        SpaceReport {
            root: node(
                "/h",
                NodeKind::Dir,
                100,
                10,
                vec![
                    node(
                        "/h/a",
                        NodeKind::Dir,
                        60,
                        6,
                        vec![
                            node("/h/a/deep", NodeKind::Dir, 50, 5, vec![]),
                            node("/h/a/f", NodeKind::File, 10, 1, vec![]),
                        ],
                    ),
                    node("/h/b", NodeKind::File, 40, 1, vec![]),
                ],
            ),
            large_files: vec![rind_core::space::LargeFile {
                path: p("/h/b"),
                size: 40,
                modified: None,
                accessed: None,
                hold_back: None,
            }],
            unreadable: vec![],
            incomplete: false,
        }
    }

    #[test]
    fn pruning_space_subtracts_from_ancestors() {
        let moved: HashSet<PathBuf> = [p("/h/a/deep"), p("/h/b")].into();
        let out = prune_space(&tree(), &moved);
        assert_eq!(out.root.size, Size::Known(10));
        assert_eq!(out.root.files, 4);
        assert_eq!(out.root.children.len(), 1);
        assert_eq!(out.root.children[0].size, Size::Known(10));
        assert!(out.large_files.is_empty());
    }

    #[test]
    fn locate_prefers_an_expanded_copy() {
        let main = tree();
        assert!(needs_drill(main.find(Path::new("/h/a/deep")).unwrap()));
        let sub = SpaceReport {
            root: node(
                "/h/a/deep",
                NodeKind::Dir,
                50,
                5,
                vec![node("/h/a/deep/x", NodeKind::File, 50, 5, vec![])],
            ),
            large_files: vec![],
            unreadable: vec![],
            incomplete: false,
        };
        let found = locate([&main, &sub], Path::new("/h/a/deep")).unwrap();
        assert_eq!(found.children.len(), 1);
        let only_main = locate([&main], Path::new("/h/a/deep")).unwrap();
        assert!(only_main.children.is_empty());
        assert!(locate([&main], Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn breadcrumbs_walk_from_root() {
        assert_eq!(
            breadcrumbs(Path::new("/h"), Path::new("/h/a/b")),
            vec![p("/h"), p("/h/a"), p("/h/a/b")]
        );
        assert_eq!(breadcrumbs(Path::new("/h"), Path::new("/h")), vec![p("/h")]);
        assert_eq!(breadcrumbs(Path::new("/h"), Path::new("/x")), vec![p("/h")]);
    }

    #[test]
    fn share_and_progress() {
        assert_eq!(share(Size::Known(25), Size::Known(100)), 0.25);
        assert_eq!(share(Size::Unknown, Size::Known(100)), 0.0);
        assert_eq!(share(Size::Known(1), Size::Known(0)), 0.0);
        assert_eq!(
            progress_percent(&ScanProgress::Measuring { done: 1, total: 4 }),
            Some(25.0)
        );
        assert_eq!(
            progress_percent(&ScanProgress::Measuring { done: 9, total: 0 }),
            None
        );
        assert_eq!(progress_percent(&ScanProgress::Finished), None);
    }

    fn launch(plist: &str, domain: LaunchDomain, orphan: bool, label: Option<&str>) -> LaunchItem {
        LaunchItem {
            plist: p(plist),
            domain,
            label: label.map(String::from),
            program: None,
            program_exists: !orphan,
            run_at_load: true,
            keep_alive: false,
            owner: None,
            orphan,
            status: LaunchStatus::NotLoaded,
            hold_back: match domain {
                LaunchDomain::UserAgent => None,
                _ => Some(HoldBack::NeedsAdmin),
            },
        }
    }

    #[test]
    fn launch_filters_and_names() {
        let items = vec![
            launch("/u/a.plist", LaunchDomain::UserAgent, false, Some("com.a")),
            launch("/u/b.plist", LaunchDomain::UserAgent, true, None),
            launch(
                "/L/c.plist",
                LaunchDomain::GlobalDaemon,
                true,
                Some("com.c"),
            ),
        ];
        let count = |f: LaunchFilter| items.iter().filter(|i| f.matches(i)).count();
        assert_eq!(count(LaunchFilter::All), 3);
        assert_eq!(count(LaunchFilter::Orphans), 2);
        assert_eq!(count(LaunchFilter::User), 2);
        assert_eq!(launch_name(&items[0]), "com.a");
        assert_eq!(launch_name(&items[1]), "b");
        assert!(items[0].manageable());
        assert!(
            !items[1].manageable(),
            "no label, nothing to tell launchctl"
        );
        let moved: HashSet<PathBuf> = [p("/u/b.plist")].into();
        assert_eq!(prune_launch(&items, &moved).len(), 2);
    }

    #[test]
    fn page_groups_cover_every_page_once() {
        let mut all: Vec<Page> = vec![Page::Overview];
        all.extend(Page::CLEAN);
        all.extend(Page::FOOTER);
        let unique: HashSet<Page> = all.iter().copied().collect();
        assert_eq!(unique.len(), 8);
        assert_eq!(page_from_name("Space"), Some(Page::Space));
        assert_eq!(page_from_name("nope"), None);
    }
}
