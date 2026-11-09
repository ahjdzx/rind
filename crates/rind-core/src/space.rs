//! 空间分析：一个目录下什么最占地方，以及大文件清单。思路见 docs/ARCHITECTURE.md「空间」一节。
//!
//! 契约（GUI / CLI 依赖它，改动要同步）：
//! - 大小一律按实际占用块数（与 `size` 模块一致），硬链接只算一次；不跨卷；不跟随符号链接。
//! - 根先解析成盘上的真实路径（祖先里的符号链接、大小写都还原），输出里的每个 `path` 都是真实路径，
//!   移动出口拿它解析后应当完全相等。根自己是符号链接时只报它自己。
//! - 分析 `/`：系统卷和数据卷是一个卷组，数据卷经 firmlink（`/Users`、`/Applications`、`/private`……）出现在根下，
//!   同时又挂在 `/System/Volumes/Data`。数据卷的设备号也算同卷（老系统上两者不同），但跳过 `/System/Volumes/Data`
//!   本身，否则数据卷要算两遍。真正的其它卷（`/Volumes/*`、`/System/Volumes/VM` 等）照样不进。
//!   直接分析 `/System/Volumes/Data` 也行，只是那下面都在 `/System` 里，全部按系统路径保护。
//! - 树只保留到 `max_depth` 层，每层按大小降序最多 `max_children` 个子项，其余并成一个
//!   `SpaceNode { kind: Other }`；想往深处看就对子目录再调一次 `analyze`。
//! - 读不了的目录记进 `unreadable`，节点大小只算读到的部分并标 `partial`。
//! - 每个节点都带 `hold_back`（保护区 / 系统路径），界面据此禁止删除。子树里有不能选的东西（受保护、macOS 自己的、
//!   要管理员、在用、本身又含不能动的）时，它的每个祖先（含根）都标 `Contains { path, why }`：`path` 是离它最近的
//!   那个真正挡路的东西（多层时直接指到它，不套娃）。树里看不到的部分（超过 `max_depth`、并进 `Other` 的）由走树时
//!   的便宜检查补上：照片 / 音乐库、固定的保护位置、`com.apple.*` 等 macOS 自己的东西、带钱包的浏览器 profile。
//!   读不了（`Unreadable`）本身可选，不往上传。
//! - 超时或取消：没走完的目录、没来得及核实的节点都按 `Protected { Unresolvable }` 处理（不可选），宁可多拦，
//!   不让没核实过的东西变成可选。重新分析即可。
//! - 硬链接：同一个 inode 只算给最先读到它的那个名字。多线程下是哪个名字不确定（总数确定）；要固定归属得先收齐
//!   所有名字再排序，内存代价不值。记 inode 的集合有上限，满了以后新的多链接文件不再去重，总大小只会偏大。

use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::leftovers::{accessible, needs_admin_as};
use crate::remove::Removable;
use crate::{protect, Env, HoldBack, ProtectReason, ScanProgress, Size};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Dir,
    File,
    /// 被折叠的小项之和。
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceNode {
    pub path: PathBuf,
    pub name: String,
    pub kind: NodeKind,
    pub size: Size,
    /// 子树里的文件数。
    pub files: u64,
    /// 有读不了的子目录、或超时 / 取消，大小只是下限。
    pub partial: bool,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    pub hold_back: Option<HoldBack>,
    /// 按大小降序（`Other` 总在最后）。超过 `max_depth` 的目录这里是空的，`kind` 仍是 `Dir`。
    pub children: Vec<SpaceNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    pub accessed: Option<chrono::DateTime<chrono::Utc>>,
    pub hold_back: Option<HoldBack>,
}

#[derive(Clone, Debug)]
pub struct SpaceOptions {
    pub max_depth: usize,
    pub max_children: usize,
    /// 多大算大文件。
    pub large_file_min: u64,
    /// 大文件清单最多多少个（取最大的）。
    pub large_file_limit: usize,
    pub budget: Duration,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for SpaceOptions {
    fn default() -> Self {
        SpaceOptions {
            max_depth: 3,
            max_children: 40,
            // 十进制，与 `format_bytes` 一致：显示出来正好是 100 MB。
            large_file_min: 100_000_000,
            large_file_limit: 200,
            budget: Duration::from_secs(120),
            cancel: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceReport {
    pub root: SpaceNode,
    /// 按大小降序。
    pub large_files: Vec<LargeFile>,
    /// 读不了的目录（最多记几百个）。
    pub unreadable: Vec<PathBuf>,
    /// 超时或被取消，结果不完整。
    pub incomplete: bool,
}

impl SpaceReport {
    /// 树里（不含根和 `Other`）和大文件清单里能被移走的项，按路径去重，树里的在前。
    /// `partial` 的节点大小只是下限，这里记为 `Unknown`：移走能释放多少说不准，不能当成已知。
    pub fn removables(&self) -> Vec<Removable> {
        fn walk(node: &SpaceNode, seen: &mut HashSet<PathBuf>, out: &mut Vec<Removable>) {
            for child in &node.children {
                if child.kind == NodeKind::Other {
                    continue;
                }
                if seen.insert(child.path.clone()) {
                    out.push(Removable {
                        path: child.path.clone(),
                        size: if child.partial {
                            Size::Unknown
                        } else {
                            child.size
                        },
                        hold_back: child.hold_back.clone(),
                    });
                }
                walk(child, seen, out);
            }
        }
        // 根先占位：它永远不是候选，哪怕大文件清单里出现同一路径。
        let mut seen: HashSet<PathBuf> = [self.root.path.clone()].into();
        let mut out = Vec::new();
        walk(&self.root, &mut seen, &mut out);
        for file in &self.large_files {
            if seen.insert(file.path.clone()) {
                out.push(Removable {
                    path: file.path.clone(),
                    size: Size::Known(file.size),
                    hold_back: file.hold_back.clone(),
                });
            }
        }
        out
    }

    /// 按路径找节点（GUI 下钻用）。
    pub fn find(&self, path: &Path) -> Option<&SpaceNode> {
        fn walk<'a>(node: &'a SpaceNode, path: &Path) -> Option<&'a SpaceNode> {
            if node.path == path {
                return Some(node);
            }
            if !path.starts_with(&node.path) {
                return None;
            }
            node.children.iter().find_map(|c| walk(c, path))
        }
        walk(&self.root, path)
    }
}

/// 读不了的目录最多记这么多条：再多界面也列不完，内存也不该跟着涨。
const UNREADABLE_LIMIT: usize = 500;
/// 进度最多每这么多毫秒报一次，免得界面被回调淹没。
const PROGRESS_EVERY_MS: u64 = 100;
/// 一个目录里每看这么多项就查一次停止条件和进度，巨型目录里也能及时停。
const CHECK_EVERY: usize = 256;
/// 分析线程的栈。目录可能嵌套几百层，每层一次 rayon 递归；栈按需提交，给大一点不占实际内存。
const STACK_SIZE: usize = 64 * 1024 * 1024;
/// 折叠项的名字；界面按 `NodeKind::Other` 自己翻译。
const OTHER_NAME: &str = "…";
/// 记多链接文件 inode 的集合最多这么多项（每项十几字节，约十几 MB）。满了就不再去重，大小只会偏大。
const LINKS_LIMIT: usize = 500_000;
/// 走树把预算用完了，保护判断再多给这么久：它决定能不能选，不该因为树大就整片按未核实处理。取消则立即停。
const HOLD_BACK_GRACE: Duration = Duration::from_secs(5);
/// 数据卷的挂载点；分析 `/` 时它经 firmlink 已经出现在根下，这里不再进去。
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// 分析 `root`（必须是绝对路径的目录）。进度用 `ScanProgress::Location` 报当前目录，
/// `Measuring { done, total }` 报已看过的文件数（`total` 未知时为 0）。
///
/// 超时或取消时尽快停下，已经读到的部分照样返回（`incomplete = true`，没读完的节点 `partial`）：
/// 和卸载时的度量不同，这里"至少这么大"对用户也有用。
pub fn analyze(
    env: &Env,
    root: &Path,
    opts: &SpaceOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> SpaceReport {
    let report = run(env, root, opts, on_progress);
    on_progress(ScanProgress::Finished);
    report
}

fn run(
    env: &Env,
    root: &Path,
    opts: &SpaceOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> SpaceReport {
    // 先换成盘上的真实路径：后面每个子路径都由它拼出来，也就都是真实路径。
    let real = if root.is_absolute() {
        protect::resolve(env, root).ok()
    } else {
        None
    };
    let root = real.clone().unwrap_or_else(|| root.to_path_buf());
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let meta = match real.as_deref().map(std::fs::symlink_metadata) {
        Some(Ok(meta)) => meta,
        failed => {
            // 根本身都看不了：大小未知，不是 0。
            let denied =
                matches!(&failed, Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound);
            let unreadable = if denied {
                vec![root.clone()]
            } else {
                Vec::new()
            };
            return SpaceReport {
                root: SpaceNode {
                    path: root,
                    name,
                    kind: NodeKind::Dir,
                    size: Size::Unknown,
                    files: 0,
                    partial: true,
                    modified: None,
                    // 解析不了（相对路径、`..`……）的不能当成普通目录。
                    hold_back: real.is_none().then(unverified),
                    children: Vec::new(),
                },
                large_files: Vec::new(),
                unreadable,
                incomplete: false,
            };
        }
    };
    let own = meta.blocks() * 512;
    let mut node = SpaceNode {
        path: root.clone(),
        name,
        kind: if meta.is_dir() {
            NodeKind::Dir
        } else {
            NodeKind::File
        },
        size: Size::Known(own),
        files: u64::from(!meta.is_dir()),
        partial: false,
        modified: time(meta.modified()),
        hold_back: None,
        children: Vec::new(),
    };
    if !meta.is_dir() {
        // 不跟随符号链接：根是文件或链接就只报它自己。
        node.hold_back = hold_back_for(env, &root);
        return SpaceReport {
            root: node,
            large_files: Vec::new(),
            unreadable: Vec::new(),
            incomplete: false,
        };
    }

    let started = Instant::now();
    let deadline = started.checked_add(opts.budget);
    let at_slash = root == Path::new("/");
    let walk = Walk {
        env,
        opts,
        on_progress,
        root_dev: meta.dev(),
        data_dev: at_slash
            .then(|| std::fs::symlink_metadata(DATA_VOLUME).ok().map(|m| m.dev()))
            .flatten(),
        skip: at_slash.then(|| PathBuf::from(DATA_VOLUME)),
        sentinels: sentinels(env),
        libraries: [&env.user_library, &env.system_library]
            .into_iter()
            .map(|l| protect::resolve(env, l).unwrap_or_else(|_| l.clone()))
            .collect(),
        started,
        deadline,
        stopped: AtomicBool::new(false),
        next_report_ms: AtomicU64::new(0),
        files_seen: AtomicU64::new(0),
        links: Mutex::new(HashSet::new()),
        large: Mutex::new(BinaryHeap::new()),
        unreadable: Mutex::new(Vec::new()),
    };
    let work = || {
        let (tally, kids) = walk.scan_dir(&root, 0);
        let checker = Checker {
            env,
            cancel: opts.cancel.as_deref(),
            deadline: deadline.map(|d| {
                Instant::now()
                    .checked_add(HOLD_BACK_GRACE)
                    .map_or(d, |grace| d.max(grace))
            }),
            stopped: AtomicBool::new(false),
        };
        node.size = Size::Known(own + tally.bytes);
        node.files = tally.files;
        node.partial = tally.partial;
        let draft = Draft {
            node,
            kids,
            inside: tally.blocked,
            cut: tally.cut,
        };
        let heap = std::mem::take(&mut *lock(&walk.large));
        // `Reverse` 的升序就是大小降序。
        let mut large_files: Vec<LargeFile> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse((size, path, modified, accessed))| LargeFile {
                path,
                size,
                modified,
                accessed,
                hold_back: None,
            })
            .collect();
        // 保护判断要读盘（解析路径、找钱包），放在同一个池里并行做，受取消 / 预算控制。
        let ((node, _), ()) = rayon::join(
            || checker.finish(draft),
            || {
                large_files
                    .par_iter_mut()
                    .for_each(|f| f.hold_back = checker.hold_back(&f.path));
            },
        );
        (node, large_files)
    };
    // 专用池：要更大的栈；也不和调用方的全局池抢。建不出来就退回全局池。
    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(STACK_SIZE)
        .thread_name(|i| format!("rind-space-{i}"))
        .build();
    let (node, large_files) = match pool {
        Ok(pool) => pool.install(work),
        Err(_) => work(),
    };
    SpaceReport {
        root: node,
        large_files,
        incomplete: walk.stopped.load(Ordering::Relaxed),
        unreadable: walk
            .unreadable
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    }
}

/// 子树里一个不能选的东西：(路径的层数, 路径, 原因)。取"最近"按 (层数, 路径) 最小，结果与线程调度无关。
type Blocker = (usize, PathBuf, HoldBack);

/// 两个候选里离得近的那个；一样近时留 `a`（调用方把更准的放在前面）。
fn nearer(a: Option<Blocker>, b: Option<Blocker>) -> Option<Blocker> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if (b.0, &b.1) < (a.0, &a.1) { b } else { a }),
        (a, b) => a.or(b),
    }
}

fn blocker(path: &Path, why: HoldBack) -> Blocker {
    (path.components().count(), path.to_path_buf(), why)
}

/// 没来得及核实的节点：不可选。
fn unverified() -> HoldBack {
    HoldBack::Protected {
        why: ProtectReason::Unresolvable,
    }
}

/// 一棵子树的汇总：占用字节、文件数、有没有没读到的部分。
#[derive(Default)]
struct Tally {
    bytes: u64,
    files: u64,
    partial: bool,
    /// 因为取消 / 超时没走完（读不了不算）。
    cut: bool,
    /// 子树里（不含目录自己）最近的不能选的东西，走树时的便宜检查找到的。
    blocked: Option<Blocker>,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.bytes += other.bytes;
        self.files += other.files;
        self.partial |= other.partial;
        self.cut |= other.cut;
        self.blocked = nearer(self.blocked.take(), other.blocked);
    }
}

/// 还没算 `hold_back` 的树节点。`node.children` 先空着，最后由 `kids` 填。
struct Draft {
    node: SpaceNode,
    kids: Vec<Draft>,
    /// 子树里（不含自己）走树时找到的最近的不能选的东西，含树里看不到的深处。
    inside: Option<Blocker>,
    /// 没走完。
    cut: bool,
}

impl Draft {
    fn leaf(node: SpaceNode) -> Self {
        Draft {
            node,
            kids: Vec::new(),
            inside: None,
            cut: false,
        }
    }
}

/// 大的在前；一样大按名字。
fn rank(a: &SpaceNode, b: &SpaceNode) -> CmpOrdering {
    let size = |n: &SpaceNode| n.size.known().unwrap_or(0);
    size(b).cmp(&size(a)).then_with(|| a.name.cmp(&b.name))
}

/// 堆里的一项。按 `rank` 比较，所以大顶堆的堆顶是排得最靠后、最先被挤掉的那个。
struct Ranked(Draft);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == CmpOrdering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        rank(&self.0.node, &other.0.node)
    }
}

/// 一个目录里排前 `max` 的子项：边枚举边淘汰，被挤掉的只记总数，最后并成一个 `Other`。
/// 巨型目录里内存只跟 `max` 有关，不跟子项数走。
struct Top {
    max: usize,
    heap: BinaryHeap<Ranked>,
    /// 被挤掉的：(个数, 字节, 文件数, 有没有 partial)。
    rest: (usize, u64, u64, bool),
}

impl Top {
    fn new(max: usize) -> Self {
        Top {
            max,
            heap: BinaryHeap::new(),
            rest: (0, 0, 0, false),
        }
    }

    fn offer(&mut self, draft: Draft) {
        if self.heap.len() < self.max {
            self.heap.push(Ranked(draft));
            return;
        }
        let out = match self.heap.peek_mut() {
            Some(mut worst) if rank(&draft.node, &worst.0.node) == CmpOrdering::Less => {
                std::mem::replace(&mut worst.0, draft)
            }
            _ => draft,
        };
        self.rest.0 += 1;
        self.rest.1 += out.node.size.known().unwrap_or(0);
        self.rest.2 += out.node.files;
        self.rest.3 |= out.node.partial;
    }

    fn merge(&mut self, other: Top) {
        self.rest.0 += other.rest.0;
        self.rest.1 += other.rest.1;
        self.rest.2 += other.rest.2;
        self.rest.3 |= other.rest.3;
        for Ranked(draft) in other.heap {
            self.offer(draft);
        }
    }

    /// 降序排好；有被挤掉的就在最后放一个 `Other`。
    fn finish(self, parent: &Path) -> Vec<Draft> {
        // `into_sorted_vec` 是升序，按 `rank` 就是从大到小。
        let mut kids: Vec<Draft> = self
            .heap
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let (count, bytes, files, partial) = self.rest;
        if count > 0 {
            kids.push(Draft::leaf(SpaceNode {
                // 折叠项不是真实路径；用父目录，`find` 会先命中父目录本身。
                path: parent.to_path_buf(),
                name: OTHER_NAME.to_string(),
                kind: NodeKind::Other,
                size: Size::Known(bytes),
                files,
                partial,
                modified: None,
                hold_back: None,
                children: Vec::new(),
            }));
        }
        kids
    }
}

/// 大文件堆里的一项：(大小, 路径, 修改时间, 访问时间)。元组按大小优先比较，正好给小顶堆用。
type Big = (
    u64,
    PathBuf,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// 待扫的子目录：只留后面要用的，不留整份 `Metadata`。
struct Sub {
    path: PathBuf,
    /// 目录自己的块。
    bytes: u64,
    /// 只有要做成节点时才取。
    modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// 正在扫的目录的一点上下文，每个目录算一次。
#[derive(Clone, Copy)]
struct DirCtx {
    /// 是资源库或它的一层子目录：这里面普通名字的系统服务目录也算 macOS 自己的。
    in_library: bool,
    /// 名字是 `extensions`（Firefox profile 里放扩展的地方）。
    extensions: bool,
}

/// 一次分析的共享状态，各线程只通过原子量和短锁碰它。
struct Walk<'a> {
    env: &'a Env,
    opts: &'a SpaceOptions,
    on_progress: &'a (dyn Fn(ScanProgress) + Sync),
    root_dev: u64,
    /// 分析 `/` 时数据卷的设备号，也算同卷。
    data_dev: Option<u64>,
    /// 分析 `/` 时不进的 `/System/Volumes/Data`。
    skip: Option<PathBuf>,
    /// 固定的保护位置（已解析）和它们的名字：先比名字再比整条路径，命中了再问 `protect::check`。
    sentinels: Vec<(OsString, PathBuf)>,
    /// 用户和系统的资源库（已解析）。
    libraries: Vec<PathBuf>,
    started: Instant,
    /// `None`：预算大到溢出，等于不限时。
    deadline: Option<Instant>,
    stopped: AtomicBool,
    /// 下一次允许报进度的时刻（相对 `started` 的毫秒）。
    next_report_ms: AtomicU64,
    files_seen: AtomicU64,
    /// 见过的多链接文件；`nlink == 1` 的不进来，免得集合跟着文件数涨。最多 `LINKS_LIMIT` 项。
    links: Mutex<HashSet<(u64, u64)>>,
    /// 小顶堆，只留最大的 `large_file_limit` 个。
    large: Mutex<BinaryHeap<Reverse<Big>>>,
    unreadable: Mutex<Vec<PathBuf>>,
}

impl Walk<'_> {
    /// 扫一个目录（不含它自己的块）。`depth` 是它在树里的层数，根是 0；
    /// `depth < max_depth` 时才把子项做成节点（每层只留前 `max_children` 个），更深处只汇总，内存不随文件数涨。
    fn scan_dir(&self, dir: &Path, depth: usize) -> (Tally, Vec<Draft>) {
        let keep = depth < self.opts.max_depth;
        let mut tally = Tally::default();
        if self.should_stop() {
            tally.partial = true;
            tally.cut = true;
            return (tally, Vec::new());
        }
        self.report(dir);
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // 扫的时候被删了：不算读不了。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (tally, Vec::new()),
            Err(_) => {
                self.note_unreadable(dir);
                tally.partial = true;
                return (tally, Vec::new());
            }
        };
        let ctx = DirCtx {
            in_library: self
                .libraries
                .iter()
                .any(|l| same_path_ci(l, dir) || dir.parent().is_some_and(|p| same_path_ci(l, p))),
            extensions: dir
                .file_name()
                .is_some_and(|n| n.as_encoded_bytes().eq_ignore_ascii_case(b"extensions")),
        };

        let max = self.opts.max_children;
        let mut top = Top::new(max);
        let mut subdirs = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index % CHECK_EVERY == CHECK_EVERY - 1 {
                if self.should_stop() {
                    tally.partial = true;
                    tally.cut = true;
                    break;
                }
                self.report(dir);
            }
            let Ok(entry) = entry else {
                tally.partial = true;
                continue;
            };
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    tally.partial = true;
                    continue;
                }
            };
            // 挂载点下面是另一个卷：不进，也不计。
            if meta.dev() != self.root_dev && Some(meta.dev()) != self.data_dev {
                continue;
            }
            let path = entry.path();
            if meta.is_dir() && self.skip.as_deref() == Some(path.as_path()) {
                continue;
            }
            let name = entry.file_name();
            if let Some(why) = self.quick_block(&path, &name, meta.is_dir(), ctx) {
                tally.blocked = nearer(tally.blocked.take(), Some(blocker(&path, why)));
            }
            if meta.is_dir() {
                subdirs.push(Sub {
                    path,
                    bytes: meta.blocks() * 512,
                    modified: if keep { time(meta.modified()) } else { None },
                });
                continue;
            }
            // 符号链接按它自己的块计（通常是 0），不跟随。
            if meta.nlink() > 1 && !self.first_link(meta.dev(), meta.ino()) {
                continue;
            }
            let bytes = meta.blocks() * 512;
            tally.bytes += bytes;
            tally.files += 1;
            self.offer_large(&path, bytes, &meta);
            if keep {
                top.offer(Draft::leaf(SpaceNode {
                    name: name.to_string_lossy().into_owned(),
                    path,
                    kind: NodeKind::File,
                    size: Size::Known(bytes),
                    files: 1,
                    partial: false,
                    modified: time(meta.modified()),
                    hold_back: None,
                    children: Vec::new(),
                }));
            }
        }
        self.files_seen.fetch_add(tally.files, Ordering::Relaxed);

        // 按目录分治：每个子目录一个任务，rayon 的工作窃取负责把宽窄不一的树摊开。
        // 结果边算边并进各自的 `Top`，不先收齐成一个大 Vec。
        let (subs, kept) = subdirs
            .into_par_iter()
            .map(|sub| {
                let (inner, grandchildren) = self.scan_dir(&sub.path, depth + 1);
                let draft = keep.then(|| Draft {
                    node: SpaceNode {
                        name: sub
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: sub.path,
                        kind: NodeKind::Dir,
                        size: Size::Known(sub.bytes + inner.bytes),
                        files: inner.files,
                        partial: inner.partial,
                        modified: sub.modified,
                        hold_back: None,
                        children: Vec::new(),
                    },
                    kids: grandchildren,
                    inside: inner.blocked.clone(),
                    cut: inner.cut,
                });
                let total = Tally {
                    bytes: sub.bytes + inner.bytes,
                    ..inner
                };
                (total, draft)
            })
            .fold(
                || (Tally::default(), Top::new(max)),
                |(mut tally, mut top), (sub, draft)| {
                    tally.add(sub);
                    if let Some(draft) = draft {
                        top.offer(draft);
                    }
                    (tally, top)
                },
            )
            .reduce(
                || (Tally::default(), Top::new(max)),
                |(mut a, mut top_a), (b, top_b)| {
                    a.add(b);
                    top_a.merge(top_b);
                    (a, top_a)
                },
            );
        tally.add(subs);
        if !keep {
            return (tally, Vec::new());
        }
        top.merge(kept);
        (tally, top.finish(dir))
    }

    /// 走树时的便宜检查：这一项是不是明摆着不能动。只看名字和少数固定位置，不解析路径、不搜钱包，
    /// 命中候选时才问 `protect`。用来给树里看不到的深处补上 `Contains`；树里的节点另有完整判断。
    fn quick_block(
        &self,
        path: &Path,
        name: &OsStr,
        is_dir: bool,
        ctx: DirCtx,
    ) -> Option<HoldBack> {
        let n = name.as_encoded_bytes();
        if ends_with_ci(n, b".photoslibrary") || ends_with_ci(n, b".aplibrary") {
            return Some(HoldBack::Protected {
                why: ProtectReason::PhotoLibrary,
            });
        }
        if ends_with_ci(n, b".musiclibrary") || ends_with_ci(n, b".tvlibrary") {
            return Some(HoldBack::Protected {
                why: ProtectReason::MusicLibrary,
            });
        }
        let sentinel = self.sentinels.iter().any(|(sentinel_name, sentinel)| {
            sentinel_name.as_encoded_bytes().eq_ignore_ascii_case(n) && same_path_ci(sentinel, path)
        });
        // 钱包：Chromium 的 `Local Extension Settings` 目录、Firefox `extensions` 里的 xpi。
        // `protect::check` 会往上找所在的 profile；问文件或这个小目录，不会引发大范围的往下搜。
        let wallet = (is_dir && n.eq_ignore_ascii_case(b"Local Extension Settings"))
            || (!is_dir && ctx.extensions && ends_with_ci(n, b".xpi"));
        if sentinel || wallet {
            if let Some(why) = protect::check(self.env, path) {
                return Some(HoldBack::Protected { why });
            }
        }
        let apple = starts_with_ci(n, b"com.apple") || starts_with_ci(n, b"group.com.apple");
        if (apple || ctx.in_library) && protect::is_system_owned(self.env, path) {
            return Some(HoldBack::SystemOwned);
        }
        None
    }

    /// 这个多链接文件是第一次见吗。集合满了就不再记：没见过的一律当第一次（大小偏大，不会偏小）。
    fn first_link(&self, dev: u64, ino: u64) -> bool {
        let mut links = lock(&self.links);
        if links.contains(&(dev, ino)) {
            return false;
        }
        if links.len() < LINKS_LIMIT {
            links.insert((dev, ino));
        }
        true
    }

    /// 取消或超时了吗？一旦是，之后所有线程都直接收手。
    fn should_stop(&self) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return true;
        }
        let cancelled = self
            .opts
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed));
        let late = self.deadline.is_some_and(|d| Instant::now() >= d);
        if cancelled || late {
            self.stopped.store(true, Ordering::Relaxed);
        }
        cancelled || late
    }

    /// 节流地报进度：每个时间窗只有抢到的那个线程报一次。
    fn report(&self, dir: &Path) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let next = self.next_report_ms.load(Ordering::Relaxed);
        if now < next
            || self
                .next_report_ms
                .compare_exchange(
                    next,
                    now.saturating_add(PROGRESS_EVERY_MS),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }
        let done = self.files_seen.load(Ordering::Relaxed);
        (self.on_progress)(ScanProgress::Location(self.env.display(dir)));
        (self.on_progress)(ScanProgress::Measuring {
            done: usize::try_from(done).unwrap_or(usize::MAX),
            total: 0,
        });
    }

    fn note_unreadable(&self, dir: &Path) {
        let mut list = lock(&self.unreadable);
        if list.len() < UNREADABLE_LIMIT {
            list.push(dir.to_path_buf());
        }
    }

    fn offer_large(&self, path: &Path, bytes: u64, meta: &std::fs::Metadata) {
        let limit = self.opts.large_file_limit;
        if limit == 0 || bytes < self.opts.large_file_min {
            return;
        }
        let mut heap = lock(&self.large);
        // 满了且不比最小的大：不必分配路径。
        if heap.len() >= limit && heap.peek().is_some_and(|Reverse(min)| bytes < min.0) {
            return;
        }
        heap.push(Reverse((
            bytes,
            path.to_path_buf(),
            time(meta.modified()),
            time(meta.accessed()),
        )));
        if heap.len() > limit {
            heap.pop();
        }
    }
}

/// 第二步：给树里每个真实节点算 `hold_back`，再把子树里不能选的东西往上传成 `Contains`。
/// 受取消 / 预算控制：停下以后没算的节点一律按未核实处理（不可选），所以停得再早也不会把受保护的东西放成可选。
struct Checker<'a> {
    env: &'a Env,
    cancel: Option<&'a AtomicBool>,
    deadline: Option<Instant>,
    stopped: AtomicBool,
}

impl Checker<'_> {
    fn should_stop(&self) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return true;
        }
        let stop = self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            || self.deadline.is_some_and(|d| Instant::now() >= d);
        if stop {
            self.stopped.store(true, Ordering::Relaxed);
        }
        stop
    }

    /// 一个路径自己的阻拦；来不及算的按未核实处理。
    fn hold_back(&self, path: &Path) -> Option<HoldBack> {
        if self.should_stop() {
            return Some(unverified());
        }
        hold_back_for(self.env, path)
    }

    /// 返回做好的节点，以及它自己或子树里最近的不能选的东西（给父节点用）。
    fn finish(&self, draft: Draft) -> (SpaceNode, Option<Blocker>) {
        let Draft {
            mut node,
            kids,
            inside,
            cut,
        } = draft;
        // `Other` 不是路径，不算；它里面有什么已经随走树的汇总记在父节点的 `inside` 里。
        if node.kind == NodeKind::Other {
            return (node, None);
        }
        let (own, done): (Option<HoldBack>, Vec<(SpaceNode, Option<Blocker>)>) = rayon::join(
            || self.hold_back(&node.path),
            || kids.into_par_iter().map(|k| self.finish(k)).collect(),
        );
        let mut nearest = None;
        node.children = done
            .into_iter()
            .map(|(child, found)| {
                nearest = nearer(nearest.take(), found);
                child
            })
            .collect();
        // 树里算出来的更准，放在前面；走树找到的补上看不到的部分。
        nearest = nearer(nearest, inside);
        // 没走完的目录：里面有什么不知道，不能让它可选。
        let own = match own {
            Some(h) if !h.selectable() => Some(h),
            _ if cut => Some(unverified()),
            other => other,
        };
        node.hold_back = match (own, nearest) {
            (Some(h), _) if !h.selectable() => Some(h),
            (_, Some((_, path, why))) => Some(HoldBack::Contains {
                path,
                why: Box::new(why),
            }),
            (own, None) => own,
        };
        let up = match &node.hold_back {
            Some(HoldBack::Contains { path, why }) => Some(blocker(path, (**why).clone())),
            Some(h) if !h.selectable() => Some(blocker(&node.path, h.clone())),
            _ => None,
        };
        (node, up)
    }
}

/// 与卸载一致的阻拦顺序：保护区 → macOS 自己的 → 要管理员 → 读不了。
fn hold_back_for(env: &Env, path: &Path) -> Option<HoldBack> {
    if let Some(why) = protect::check(env, path) {
        return Some(HoldBack::Protected { why });
    }
    if protect::is_system_owned(env, path) {
        return Some(HoldBack::SystemOwned);
    }
    // SAFETY: geteuid 没有参数，总是成功。
    if needs_admin_as(path, unsafe { libc::geteuid() }) {
        return Some(HoldBack::NeedsAdmin);
    }
    if !accessible(path, libc::R_OK) {
        return Some(HoldBack::Unreadable);
    }
    None
}

/// 走树时要认出来的固定保护位置：`protect` 的保护根、家目录 / 资源库 / 应用程序这些结构性的根、系统路径、
/// Rind 自己。只是预筛：命中后的结论仍由 `protect::check` 给；这里漏了的，树里的节点还有完整判断兜底。
fn sentinels(env: &Env) -> Vec<(OsString, PathBuf)> {
    let mut paths: Vec<PathBuf> = protect::protected_roots(env)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    paths.extend([
        env.home.clone(),
        env.user_library.clone(),
        env.system_library.clone(),
        env.data_dir.clone(),
        env.receipts_dir.clone(),
        env.home.join(".Trash"),
    ]);
    paths.extend(env.app_dirs.iter().chain(&env.system_app_dirs).cloned());
    for folder in [
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Music",
        "Movies",
        "Public",
        "Applications",
    ] {
        paths.push(env.home.join(folder));
    }
    let system_root = env.system_library.parent().unwrap_or(Path::new("/"));
    for sys in [
        "System",
        "usr",
        "bin",
        "sbin",
        "etc",
        "private/etc",
        "Library/Apple",
        "var",
        "private/var",
    ] {
        paths.push(system_root.join(sys));
    }
    if let Some(bundle) = std::env::current_exe().ok().and_then(|exe| {
        exe.ancestors()
            .find(|a| a.extension().is_some_and(|e| e == "app"))
            .map(Path::to_path_buf)
    }) {
        paths.push(bundle);
    }
    let mut out: Vec<(OsString, PathBuf)> = Vec::new();
    for path in paths {
        let real = protect::resolve(env, &path).unwrap_or(path);
        let Some(name) = real.file_name().map(OsStr::to_os_string) else {
            continue;
        };
        if !out.iter().any(|(_, p)| same_path_ci(p, &real)) {
            out.push((name, real));
        }
    }
    out
}

/// 两条真实路径是不是同一个地方：逐段比较，ASCII 不区分大小写（APFS 默认如此）。
fn same_path_ci(a: &Path, b: &Path) -> bool {
    let mut a = a.components();
    let mut b = b.components();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y))
                if x.as_os_str()
                    .as_encoded_bytes()
                    .eq_ignore_ascii_case(y.as_os_str().as_encoded_bytes()) => {}
            _ => return false,
        }
    }
}

fn ends_with_ci(name: &[u8], suffix: &[u8]) -> bool {
    name.len() >= suffix.len() && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn starts_with_ci(name: &[u8], prefix: &[u8]) -> bool {
    name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn time(t: std::io::Result<std::time::SystemTime>) -> Option<chrono::DateTime<chrono::Utc>> {
    t.ok().map(Into::into)
}

/// 锁里只有简单的插入 / 弹出，别的线程 panic 了数据也还是一致的，不必跟着失败。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtectReason;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// 临时目录，`path()` 给解析后的真实路径（`/var` → `/private/var`）：分析结果里的路径都是真实路径。
    struct Tmp {
        _dir: tempfile::TempDir,
        real: PathBuf,
    }
    impl Tmp {
        fn path(&self) -> &Path {
            &self.real
        }
    }
    fn tempdir() -> Tmp {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap();
        Tmp { _dir: dir, real }
    }

    fn scan(root: &Path, opts: &SpaceOptions) -> SpaceReport {
        let dir = tempdir();
        analyze(&Env::for_test(dir.path()), root, opts, &|_| {})
    }

    fn put(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; len]).unwrap();
    }

    /// 盘上实际占用，和实现用同一口径。
    fn blocks(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    fn known(size: Size) -> u64 {
        size.known().expect("a scanned node has a known size")
    }

    fn names(node: &SpaceNode) -> Vec<&str> {
        node.children.iter().map(|c| c.name.as_str()).collect()
    }

    /// chmod 000 的目录，测试结束（哪怕 panic）都把权限改回来，tempdir 才删得掉。
    struct Locked(PathBuf);
    impl Locked {
        fn new(path: PathBuf) -> Self {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            Locked(path)
        }
    }
    impl Drop for Locked {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    fn is_root() -> bool {
        // SAFETY: geteuid 总是成功。
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn sums_block_sizes_and_sorts_children_by_size() {
        let dir = tempdir();
        let root = dir.path();
        put(&root.join("a.bin"), 10_000);
        put(&root.join("b.bin"), 50_000);
        put(&root.join("sub/c.bin"), 100_000);
        put(&root.join("sub/d.bin"), 20_000);
        let report = scan(root, &SpaceOptions::default());

        let expected: u64 = [".", "a.bin", "b.bin", "sub", "sub/c.bin", "sub/d.bin"]
            .iter()
            .map(|p| blocks(&root.join(p)))
            .sum();
        assert_eq!(known(report.root.size), expected);
        assert_eq!(report.root.files, 4);
        assert!(!report.root.partial && !report.incomplete);
        assert_eq!(names(&report.root), ["sub", "b.bin", "a.bin"]);
        let sub = &report.root.children[0];
        assert_eq!(sub.kind, NodeKind::Dir);
        assert_eq!(sub.files, 2);
        assert_eq!(
            known(sub.size),
            blocks(&root.join("sub"))
                + blocks(&root.join("sub/c.bin"))
                + blocks(&root.join("sub/d.bin"))
        );
        assert_eq!(names(sub), ["c.bin", "d.bin"]);
        assert!(sub.modified.is_some());
    }

    #[test]
    fn children_beyond_the_limit_fold_into_other() {
        let dir = tempdir();
        for (i, len) in [10_000, 20_000, 30_000, 40_000, 50_000].iter().enumerate() {
            put(&dir.path().join(format!("f{i}")), *len);
        }
        let opts = SpaceOptions {
            max_children: 2,
            ..SpaceOptions::default()
        };
        let report = scan(dir.path(), &opts);
        let kids = &report.root.children;
        assert_eq!(kids.len(), 3);
        assert_eq!(names(&report.root)[..2], ["f4", "f3"]);
        let other = &kids[2];
        assert_eq!(other.kind, NodeKind::Other);
        assert_eq!(other.files, 3);
        let small: u64 = ["f0", "f1", "f2"]
            .iter()
            .map(|n| blocks(&dir.path().join(n)))
            .sum();
        assert_eq!(known(other.size), small);
        assert_eq!(
            kids.iter().map(|c| known(c.size)).sum::<u64>() + blocks(dir.path()),
            known(report.root.size),
            "folding loses nothing"
        );
    }

    #[test]
    fn nodes_below_max_depth_have_no_children_but_full_sizes() {
        let dir = tempdir();
        let root = dir.path();
        put(&root.join("a/b/c/deep.bin"), 200_000);
        put(&root.join("a/b/mid.bin"), 30_000);
        let opts = SpaceOptions {
            max_depth: 2,
            ..SpaceOptions::default()
        };
        let report = scan(root, &opts);
        let a = &report.root.children[0];
        let b = &a.children[0];
        assert_eq!(b.name, "b");
        assert_eq!(b.kind, NodeKind::Dir);
        assert!(b.children.is_empty(), "depth 2 is the last kept level");
        assert_eq!(b.files, 2);
        let expected: u64 = ["a/b", "a/b/c", "a/b/c/deep.bin", "a/b/mid.bin"]
            .iter()
            .map(|p| blocks(&root.join(p)))
            .sum();
        assert_eq!(known(b.size), expected);
        assert_eq!(report.root.files, 2);

        let flat = scan(
            root,
            &SpaceOptions {
                max_depth: 0,
                ..SpaceOptions::default()
            },
        );
        assert!(flat.root.children.is_empty());
        assert_eq!(flat.root.size, report.root.size);
    }

    #[test]
    fn hard_links_count_once() {
        let dir = tempdir();
        let a = dir.path().join("a.bin");
        put(&a, 100_000);
        fs::hard_link(&a, dir.path().join("b.bin")).unwrap();
        let report = scan(
            dir.path(),
            &SpaceOptions {
                large_file_min: 1,
                ..SpaceOptions::default()
            },
        );
        assert_eq!(report.root.files, 1);
        assert_eq!(known(report.root.size), blocks(dir.path()) + blocks(&a));
        assert_eq!(report.root.children.len(), 1, "only one name gets a node");
        assert_eq!(report.large_files.len(), 1, "and one large-file entry");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let outside = tempdir();
        put(&outside.path().join("big.bin"), 1_000_000);
        let dir = tempdir();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let report = scan(
            dir.path(),
            &SpaceOptions {
                large_file_min: 1,
                ..SpaceOptions::default()
            },
        );
        assert_eq!(
            known(report.root.size),
            blocks(dir.path()) + blocks(&link),
            "only the link itself counts"
        );
        assert_eq!(report.root.children.len(), 1);
        assert_eq!(report.root.children[0].kind, NodeKind::File);
        assert!(report.root.children[0].children.is_empty());
        assert!(report
            .large_files
            .iter()
            .all(|f| !f.path.starts_with(outside.path())));

        // 根本身是链接也不跟随。
        let rooted = scan(&link, &SpaceOptions::default());
        assert_eq!(rooted.root.kind, NodeKind::File);
        assert!(rooted.root.children.is_empty());
    }

    #[test]
    fn large_files_respect_threshold_and_limit() {
        let dir = tempdir();
        for (name, len) in [
            ("tiny", 10_000),
            ("x/two", 200_000),
            ("y/three", 300_000),
            ("y/z/four", 400_000),
        ] {
            put(&dir.path().join(name), len);
        }
        let opts = SpaceOptions {
            large_file_min: 150_000,
            large_file_limit: 2,
            ..SpaceOptions::default()
        };
        let report = scan(dir.path(), &opts);
        let got: Vec<PathBuf> = report.large_files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            got,
            [dir.path().join("y/z/four"), dir.path().join("y/three")]
        );
        assert!(report.large_files[0].size > report.large_files[1].size);
        assert!(report
            .large_files
            .iter()
            .all(|f| f.modified.is_some() && f.accessed.is_some()));

        let none = scan(
            dir.path(),
            &SpaceOptions {
                large_file_limit: 0,
                large_file_min: 1,
                ..SpaceOptions::default()
            },
        );
        assert!(none.large_files.is_empty());
    }

    #[test]
    fn unreadable_folders_are_listed_and_mark_ancestors_partial() {
        if is_root() {
            return; // root 什么都读得了，这个用例对它没有意义
        }
        let dir = tempdir();
        let root = dir.path();
        put(&root.join("outer/locked/secret.bin"), 50_000);
        put(&root.join("outer/fine.bin"), 10_000);
        put(&root.join("other.bin"), 10_000);
        let locked = root.join("outer/locked");
        let guard = Locked::new(locked.clone());
        let report = scan(root, &SpaceOptions::default());
        drop(guard);

        assert_eq!(report.unreadable, std::slice::from_ref(&locked));
        assert!(!report.incomplete, "unreadable is not a timeout");
        assert!(report.root.partial);
        let outer = report.find(&root.join("outer")).unwrap();
        assert!(outer.partial);
        let node = report.find(&locked).unwrap();
        assert!(node.partial && node.children.is_empty());
        assert_eq!(node.hold_back, Some(HoldBack::Unreadable));
        assert!(!report.find(&root.join("other.bin")).unwrap().partial);
        assert_eq!(report.root.files, 2, "what could be read is still counted");
    }

    #[test]
    fn cancel_or_budget_returns_incomplete_partial_results() {
        let dir = tempdir();
        for i in 0..50 {
            put(&dir.path().join(format!("d{i}/f")), 1_000);
        }
        let cancel = Arc::new(AtomicBool::new(true));
        let events = Mutex::new(Vec::new());
        let report = analyze(
            &Env::for_test(dir.path()),
            dir.path(),
            &SpaceOptions {
                cancel: Some(cancel),
                ..SpaceOptions::default()
            },
            &|p| events.lock().unwrap().push(p),
        );
        assert!(report.incomplete);
        assert!(report.root.partial);
        assert!(matches!(report.root.size, Size::Known(_)), "a lower bound");
        assert_eq!(events.lock().unwrap().last(), Some(&ScanProgress::Finished));

        let late = scan(
            dir.path(),
            &SpaceOptions {
                budget: Duration::ZERO,
                ..SpaceOptions::default()
            },
        );
        assert!(late.incomplete && late.root.partial);

        let calm = scan(dir.path(), &SpaceOptions::default());
        assert!(!calm.incomplete && !calm.root.partial);
        assert_eq!(calm.root.files, 50);
    }

    #[test]
    fn reports_progress_and_finishes() {
        let dir = tempdir();
        put(&dir.path().join("a/b"), 1_000);
        let env = Env::for_test(dir.path());
        let events = Mutex::new(Vec::new());
        analyze(&env, dir.path(), &SpaceOptions::default(), &|p| {
            events.lock().unwrap().push(p)
        });
        let events = events.into_inner().unwrap();
        assert!(events.contains(&ScanProgress::Location(env.display(dir.path()))));
        assert!(events
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { total: 0, .. })));
        assert_eq!(events.last(), Some(&ScanProgress::Finished));
        assert_eq!(
            events
                .iter()
                .filter(|p| **p == ScanProgress::Finished)
                .count(),
            1
        );
    }

    #[test]
    fn a_missing_root_is_unknown_not_zero() {
        let dir = tempdir();
        let report = scan(&dir.path().join("nope"), &SpaceOptions::default());
        assert_eq!(report.root.size, Size::Unknown);
        assert!(report.root.partial);
        assert!(report.removables().is_empty());
    }

    #[test]
    fn find_walks_down_to_a_node() {
        let dir = tempdir();
        put(&dir.path().join("a/b/c.bin"), 1_000);
        let report = scan(dir.path(), &SpaceOptions::default());
        assert_eq!(report.find(dir.path()).unwrap().path, dir.path());
        let c = report.find(&dir.path().join("a/b/c.bin")).unwrap();
        assert_eq!(c.kind, NodeKind::File);
        assert!(report.find(&dir.path().join("a/zzz")).is_none());
        assert!(report.find(Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn removables_skip_root_and_other_and_dedupe_large_files() {
        let dir = tempdir();
        let root = dir.path();
        put(&root.join("big.bin"), 300_000);
        put(&root.join("small.bin"), 1_000);
        put(&root.join("sub/inner.bin"), 200_000);
        let opts = SpaceOptions {
            max_children: 2,
            large_file_min: 100_000,
            ..SpaceOptions::default()
        };
        let report = scan(root, &opts);
        assert!(report
            .root
            .children
            .iter()
            .any(|c| c.kind == NodeKind::Other));
        let items = report.removables();
        let paths: Vec<&Path> = items.iter().map(|r| r.path.as_path()).collect();
        let unique: HashSet<&Path> = paths.iter().copied().collect();
        assert_eq!(paths.len(), unique.len(), "no duplicates: {paths:?}");
        assert!(!paths.contains(&root), "the root is never a candidate");
        assert!(paths.contains(&root.join("big.bin").as_path()));
        assert!(paths.contains(&root.join("sub/inner.bin").as_path()));
        assert!(!paths.contains(&root.join("small.bin").as_path()), "folded");
        let big = items
            .iter()
            .find(|r| r.path == root.join("big.bin"))
            .unwrap();
        assert_eq!(big.size, Size::Known(blocks(&root.join("big.bin"))));

        // 树截得浅时，更深处的大文件只从大文件清单来。
        let shallow = scan(
            root,
            &SpaceOptions {
                max_depth: 1,
                ..opts
            },
        );
        assert!(shallow
            .removables()
            .iter()
            .any(|r| r.path == root.join("sub/inner.bin")));
    }

    #[test]
    fn nodes_carry_hold_backs() {
        let dir = tempdir();
        let env = Env::for_test(dir.path());
        put(&env.home.join(".ssh/id_ed25519"), 1_000);
        put(&env.home.join("com.apple.thing/x"), 1_000);
        put(&env.home.join("plain/x"), 1_000);
        let report = analyze(&env, &env.home, &SpaceOptions::default(), &|_| {});
        let hold = |p: &str| report.find(&env.home.join(p)).unwrap().hold_back.clone();
        assert_eq!(
            hold(".ssh"),
            Some(HoldBack::Protected {
                why: crate::ProtectReason::SshKeys
            })
        );
        assert_eq!(
            hold(".ssh/id_ed25519"),
            Some(HoldBack::Protected {
                why: crate::ProtectReason::SshKeys
            })
        );
        assert_eq!(hold("com.apple.thing"), Some(HoldBack::SystemOwned));
        assert_eq!(hold("plain"), None);
        assert_eq!(
            report.root.hold_back,
            Some(HoldBack::Protected {
                why: crate::ProtectReason::RootFolder
            })
        );
    }

    #[test]
    fn default_large_file_threshold_is_decimal_100_mb() {
        let min = SpaceOptions::default().large_file_min;
        assert_eq!(min, 100_000_000);
        assert_eq!(crate::size::format_bytes(min), "100 MB");
    }

    #[test]
    fn wide_directories_keep_only_the_top_children() {
        let dir = tempdir();
        let root = dir.path();
        for i in 0..600 {
            put(&root.join(format!("f{i:03}")), 1);
        }
        put(&root.join("zz-big"), 100_000);
        put(&root.join("sub/x"), 50_000);
        let report = scan(
            root,
            &SpaceOptions {
                max_children: 3,
                ..SpaceOptions::default()
            },
        );
        // 一样大的按名字，结果不随枚举顺序变。
        assert_eq!(names(&report.root), ["zz-big", "sub", "f000", "…"]);
        let other = report.root.children.last().unwrap();
        assert_eq!(other.kind, NodeKind::Other);
        assert_eq!(other.files, 599);
        assert_eq!(
            report
                .root
                .children
                .iter()
                .map(|c| known(c.size))
                .sum::<u64>()
                + blocks(root),
            known(report.root.size),
            "folding loses nothing"
        );
        assert_eq!(report.root.files, 602);
    }

    #[test]
    fn the_root_is_reported_by_its_real_path() {
        let dir = tempdir();
        let real = dir.path().join("real");
        put(&real.join("sub/a.bin"), 1_000);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // 祖先里有符号链接：换成真实位置。
        let report = scan(&link.join("sub"), &SpaceOptions::default());
        assert_eq!(report.root.path, real.join("sub"));
        assert_eq!(report.root.children[0].path, real.join("sub/a.bin"));
        // `/var` → `/private/var`。
        let raw = tempfile::tempdir().unwrap();
        put(&raw.path().join("x"), 1_000);
        let report = scan(raw.path(), &SpaceOptions::default());
        let canonical = fs::canonicalize(raw.path()).unwrap();
        assert_eq!(report.root.path, canonical);
        assert_eq!(report.root.children[0].path, canonical.join("x"));
        // 解析不了的（`..`）：不当普通目录分析，也不可选。
        let odd = scan(&dir.path().join("real/../real"), &SpaceOptions::default());
        assert_eq!(odd.root.size, Size::Unknown);
        assert!(!odd.root.hold_back.unwrap().selectable());
    }

    #[test]
    fn hidden_blockers_mark_every_ancestor_contains() {
        let dir = tempdir();
        let root = dir.path();
        let lib = root.join("pics/Trip.photoslibrary");
        put(&lib.join("database/x"), 1_000);
        put(&root.join("pics/ok.jpg"), 1_000);
        put(&root.join("plain/a.bin"), 1_000);
        let apple = root.join("deep/a/b/com.apple.thing");
        put(&apple.join("x"), 1_000);
        // 比钱包查找的深度更深：只有走树能看到。
        let wallet = root.join("w/a/b/c/d/Default/Local Extension Settings");
        put(
            &wallet.join("nkbihfbeogaeaoehlefnkodbefgpgknn/000003.log"),
            100,
        );

        let photo = HoldBack::Protected {
            why: ProtectReason::PhotoLibrary,
        };
        let contains = |path: &Path, why: HoldBack| {
            Some(HoldBack::Contains {
                path: path.to_path_buf(),
                why: Box::new(why),
            })
        };
        let hb = |r: &SpaceReport, p: &str| r.find(&root.join(p)).unwrap().hold_back.clone();

        // 截在第 1 层：库在第 2 层，树里看不到，外层照样不能选。
        let shallow = scan(
            root,
            &SpaceOptions {
                max_depth: 1,
                ..SpaceOptions::default()
            },
        );
        assert!(shallow
            .find(&root.join("pics"))
            .unwrap()
            .children
            .is_empty());
        assert_eq!(hb(&shallow, "pics"), contains(&lib, photo.clone()));
        assert_eq!(
            hb(&shallow, "deep"),
            contains(&apple, HoldBack::SystemOwned)
        );
        assert_eq!(
            hb(&shallow, "w"),
            contains(
                &wallet,
                HoldBack::Protected {
                    why: ProtectReason::Wallet
                }
            )
        );
        assert_eq!(hb(&shallow, "plain"), None);
        // 根也标上，指向最近的那个。
        assert_eq!(shallow.root.hold_back, contains(&lib, photo.clone()));
        let pics = shallow
            .removables()
            .into_iter()
            .find(|r| r.path == root.join("pics"))
            .unwrap();
        assert!(!pics.hold_back.unwrap().selectable());

        // 不截：库自己受保护，每一层祖先都指向它，旁边的照常可选。
        let full = scan(root, &SpaceOptions::default());
        assert_eq!(hb(&full, "pics/Trip.photoslibrary"), Some(photo.clone()));
        assert_eq!(hb(&full, "pics"), contains(&lib, photo.clone()));
        assert_eq!(hb(&full, "pics/ok.jpg"), None);
        assert_eq!(
            hb(&full, "deep/a/b"),
            contains(&apple, HoldBack::SystemOwned)
        );
        assert_eq!(hb(&full, "deep"), contains(&apple, HoldBack::SystemOwned));

        // 并进 `Other` 的也算。
        let folded = tempdir();
        put(&folded.path().join("big.bin"), 100_000);
        let small = folded.path().join("Small.photoslibrary");
        put(&small.join("x"), 10);
        let report = scan(
            folded.path(),
            &SpaceOptions {
                max_children: 1,
                ..SpaceOptions::default()
            },
        );
        assert_eq!(report.root.children[1].kind, NodeKind::Other);
        assert_eq!(report.root.hold_back, contains(&small, photo));
    }

    #[test]
    fn protected_places_deep_down_are_seen() {
        let dir = tempdir();
        let env = Env::for_test(dir.path());
        put(&env.home.join("notes.txt"), 1_000);
        let report = analyze(
            &env,
            dir.path(),
            &SpaceOptions {
                max_depth: 1,
                ..SpaceOptions::default()
            },
            &|_| {},
        );
        // 家目录在第 2 层：`Users` 不能整个移走。
        assert_eq!(
            report.find(&dir.path().join("Users")).unwrap().hold_back,
            Some(HoldBack::Contains {
                path: env.home.clone(),
                why: Box::new(HoldBack::Protected {
                    why: ProtectReason::RootFolder
                }),
            })
        );
    }

    #[test]
    fn stopping_midway_keeps_what_was_read_and_nothing_becomes_selectable() {
        let dir = tempdir();
        let root = dir.path();
        for i in 0..5 {
            put(&root.join(format!("r{i}")), 1_000);
        }
        for d in 0..8 {
            for f in 0..5 {
                put(&root.join(format!("d{d}/f{f}")), 1_000);
            }
            put(&root.join(format!("d{d}/n/deeper")), 1_000);
        }
        let lib = root.join("d0/n/Lib.photoslibrary");
        put(&lib.join("x"), 200_000);
        let env = Env::for_test(&tempdir().path().join("env"));
        let cancel = Arc::new(AtomicBool::new(false));
        let started = root.to_path_buf();
        // 第一次进度在根上：在那儿停一停，让下一次进度落在某个子目录里，在那时取消——遍历走到一半。
        let on_progress = |p: ScanProgress| {
            if let ScanProgress::Location(at) = p {
                if at == env.display(&started) {
                    std::thread::sleep(Duration::from_millis(150));
                } else {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        };
        let report = analyze(
            &env,
            root,
            &SpaceOptions {
                cancel: Some(cancel.clone()),
                large_file_min: 1,
                ..SpaceOptions::default()
            },
            &on_progress,
        );
        assert!(cancel.load(Ordering::Relaxed), "cancelled during the walk");
        assert!(report.incomplete && report.root.partial);
        assert!(
            report.root.files >= 5,
            "what was read before stopping counts"
        );
        // 触发取消的那个子目录：自己的文件读完了，往下的没走。
        let midway = report
            .root
            .children
            .iter()
            .find(|c| c.kind == NodeKind::Dir && c.partial && c.children.len() > 1)
            .expect("a partially walked subtree");
        assert!(midway
            .children
            .iter()
            .any(|c| c.kind == NodeKind::File && !c.partial));

        // 没核实的一律不可选：不会因为停得早，受保护的或含它的变成可选。
        fn all_blocked(node: &SpaceNode) {
            if node.kind != NodeKind::Other {
                assert!(
                    node.hold_back.as_ref().is_some_and(|h| !h.selectable()),
                    "{:?} is selectable after a cancel: {:?}",
                    node.path,
                    node.hold_back
                );
            }
            node.children.iter().for_each(all_blocked);
        }
        all_blocked(&report.root);
        assert!(report
            .large_files
            .iter()
            .all(|f| f.hold_back.as_ref().is_some_and(|h| !h.selectable())));
        assert!(report
            .removables()
            .iter()
            .all(|r| r.hold_back.as_ref().is_some_and(|h| !h.selectable())));
    }

    #[test]
    fn unreadable_is_not_passed_up() {
        if is_root() {
            return;
        }
        let dir = tempdir();
        let root = dir.path();
        put(&root.join("outer/locked/secret.bin"), 1_000);
        let guard = Locked::new(root.join("outer/locked"));
        let report = scan(root, &SpaceOptions::default());
        drop(guard);
        assert_eq!(
            report.find(&root.join("outer/locked")).unwrap().hold_back,
            Some(HoldBack::Unreadable)
        );
        assert_eq!(report.find(&root.join("outer")).unwrap().hold_back, None);
        assert_eq!(report.root.hold_back, None);
    }
}
