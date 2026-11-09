//! 所有"移到废纸篓"的唯一出口：卸载、孤儿、缓存、空间分析、启动项都走这里。
//! 规则见 docs/ARCHITECTURE.md 第 7 步：先查保护区、先记意图、只进废纸篓、写不了 History 就停。
//!
//! `refused` 里的原因是稳定的英文短语，i18n 按前缀翻译。全部前缀：
//! - `protected: <reason>`：在保护区里，`<reason>` 是 `ProtectReason` 的 snake_case 名（如 `photo_library`）
//! - `needs an administrator`
//! - `belongs to macOS`
//! - `in use by <app>`
//! - `contains <path>: <reason>`：里面有不能动的东西，`<reason>` 又是这张表里的一条
//! - `shared with <apps>`、`unreadable`：这两种本身能选，只会出现在 `contains …: ` 后面
//! - `path changed since scan`：路径现在指向的不是扫描时的那个地方（中间被换成了符号链接之类）
//! - `outside the places History can put back`
//!
//! `failed` 里是错误原文；只有一种例外：移完发现搬走的不是查过的那个东西，记 `path changed since scan`。
//!
//! 预演（`dry_run`）走同一套检查：也解析真实路径、也看目录里（往下 6 层、最多 20 万个条目）有没有照片 / 音乐库
//! 或带钱包的浏览器 profile，外层被拒时内层同样重新单独排一遍。`--dry-run --json` 说会移动的，真跑就会移动；
//! 说拒绝的，真跑也拒绝。显式的 `rind trash <目录>` 只兜底媒体库和钱包——目录里「在用 / 要管理员」的内层不会拦
//! （移动本身不需要提权，而且照样进废纸篓能放回）。

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::history::{self, Ledger};
use crate::uninstall::ExecuteError;
use crate::{
    protect, trash, Env, HistoryItem, HistoryRecord, HoldBack, ProtectReason, RecordKind,
    RemovalOutcome, Size, Source,
};

pub const REASON_PROTECTED: &str = "protected: ";
pub const REASON_NEEDS_ADMIN: &str = "needs an administrator";
pub const REASON_SYSTEM_OWNED: &str = "belongs to macOS";
pub const REASON_IN_USE: &str = "in use by ";
pub const REASON_CONTAINS: &str = "contains ";
pub const REASON_SHARED: &str = "shared with ";
pub const REASON_UNREADABLE: &str = "unreadable";
pub const REASON_CHANGED: &str = "path changed since scan";
pub const REASON_NOT_RESTORABLE: &str = "outside the places History can put back";

/// 扫描结果里能被移走的一项：路径、大小、扫描时判定的阻拦。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removable {
    pub path: PathBuf,
    pub size: Size,
    pub hold_back: Option<HoldBack>,
}

/// 真正移动之前再核一次的钩子：拿到的是解析后的真实路径，返回 `Err(why)` 就把这一项记成拒绝、不动它。
pub type Recheck<'a> = &'a dyn Fn(&Path) -> Result<(), String>;

/// 这一批是什么操作，记进 History 的哪条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub kind: RecordKind,
    /// History 里显示的标题。
    pub title: String,
    pub bundle_id: Option<String>,
    pub source: Source,
}

/// 把 `selected` 里的路径移到废纸篓。只认 `candidates`（扫描结果）里有的路径（否则 `NotInPlan`）；
/// 扫描时被挡住、现在又在保护区里、History 放不回来、路径已经变了的进 `refused`；
/// 真要动就必须有 `ledger`（`LedgerRequired`）。
/// 一个选中的路径在另一个下面时只移外层，内层随它走（也列进 `moved`）；外层里有不能动的东西时外层也拒绝。
/// 一条记录对应一批；什么都没移成就不留记录。`dry_run` 只返回将做什么。
pub fn trash_items(
    env: &Env,
    batch: &Batch,
    candidates: &[Removable],
    selected: &HashSet<PathBuf>,
    dry_run: bool,
    ledger: Option<&Ledger>,
) -> Result<RemovalOutcome, ExecuteError> {
    trash_items_checked(env, batch, candidates, selected, dry_run, ledger, None)
}

/// 同 `trash_items`，另外接受一个"真正移动之前再核一次"的回调：拿到的是解析后的真实路径，
/// 返回 `Err(why)` 就把这一项记成拒绝、不动它。启动项用它把"盘上还是核对过的那一份吗"压到最后一步
/// （写 History 之前、`rename` 之前），别的调用方不需要。
#[allow(clippy::too_many_arguments)]
pub fn trash_items_checked(
    env: &Env,
    batch: &Batch,
    candidates: &[Removable],
    selected: &HashSet<PathBuf>,
    dry_run: bool,
    ledger: Option<&Ledger>,
    recheck: Option<Recheck<'_>>,
) -> Result<RemovalOutcome, ExecuteError> {
    for path in selected {
        if !candidates.iter().any(|c| &c.path == path) {
            return Err(ExecuteError::NotInPlan(path.clone()));
        }
    }
    if selected.is_empty() {
        return Err(ExecuteError::NothingSelected);
    }
    let ledger = match (dry_run, ledger) {
        (true, _) => None,
        (false, Some(ledger)) => Some(ledger),
        (false, None) => return Err(ExecuteError::LedgerRequired),
    };

    let mut outcome = RemovalOutcome {
        dry_run,
        ..Default::default()
    };
    let all: Vec<(&Path, Option<&HoldBack>)> = candidates
        .iter()
        .map(|c| (c.path.as_path(), c.hold_back.as_ref()))
        .collect();
    let chosen: Vec<(&Path, Size, Option<&HoldBack>)> = candidates
        .iter()
        .filter(|c| selected.contains(&c.path))
        .map(|c| (c.path.as_path(), c.size, c.hold_back.as_ref()))
        .collect();
    let queue = plan_moves(env, &all, &chosen, &mut outcome.refused);

    if dry_run {
        for planned in &queue {
            pretend_checked(env, planned, &mut outcome);
        }
        return Ok(outcome);
    }
    let ledger = ledger.expect("checked above: a real run has a ledger");
    let mut mover = Mover::new(env, ledger, batch, outcome, recheck);
    for planned in queue {
        mover.move_planned(planned)?;
    }
    Ok(mover.finish())
}

/// 队列里的一项：要移动的路径，以及同样被选中、在它下面、会随它一起走的内层项。
#[derive(Debug)]
pub(crate) struct Planned {
    pub(crate) path: PathBuf,
    pub(crate) size: Size,
    pub(crate) inner: Vec<(PathBuf, Size)>,
}

impl Planned {
    /// 预演：外层和随它走的内层都算"会移走"，大小只算外层（内层已经包含在里面）。
    pub(crate) fn pretend(&self, outcome: &mut RemovalOutcome) {
        outcome.moved.push((self.path.clone(), self.path.clone()));
        for (inner, _) in &self.inner {
            outcome.moved.push((inner.clone(), inner.clone()));
        }
        outcome.freed += self.size.known().unwrap_or(0);
    }
}

/// 预演也要走一遍真跑才做的检查：解析真实路径、看目录里有没有不能一起带走的东西（照片 / 音乐库、
/// 带钱包的浏览器 profile）。不然 `--dry-run --json` 会说"要移动"，真跑却拒绝——按预演做决定的脚本就被骗了。
/// 外层被拒时和真跑一样：把它下面一起选中的内层重新单独排一遍。
fn pretend_checked(env: &Env, planned: &Planned, outcome: &mut RemovalOutcome) {
    let before = std::fs::symlink_metadata(&planned.path).ok();
    let real = match preflight(env, &planned.path) {
        Ok(real) => real,
        Err(why) => {
            outcome.refused.push((planned.path.clone(), why));
            return pretend_inner(env, planned, outcome);
        }
    };
    if before.as_ref().is_some_and(|m| m.is_dir()) {
        if let Some(why) = protect::check_descendants(&real) {
            outcome
                .refused
                .push((planned.path.clone(), protected_reason(why)));
            return pretend_inner(env, planned, outcome);
        }
    }
    planned.pretend(outcome);
}

/// 外层没走成，它下面一起选中的内层重新排一遍——先 `nest`，和真跑里 `move_planned` 对失败外层的处理一致：
/// 子项并进最外层的那一项，`freed` 才不会把内层重复计一遍。
fn pretend_inner(env: &Env, planned: &Planned, outcome: &mut RemovalOutcome) {
    for again in nest(planned.inner.clone()) {
        pretend_checked(env, &again, outcome);
    }
}

/// 选中的项 → 移动队列，拒绝的进 `refused`。`all` 是整份扫描结果（选没选的都在），用来发现外层里
/// 不能动的东西：外层下面有任何一项不可选（`hold_back` 不可选，或它被选中了却被拒绝），外层就拒绝，
/// 否则移走外层会把它一起带走。剩下的里，一项在另一项下面就并进最外层那一项。
pub(crate) fn plan_moves(
    env: &Env,
    all: &[(&Path, Option<&HoldBack>)],
    chosen: &[(&Path, Size, Option<&HoldBack>)],
    refused: &mut Vec<(PathBuf, String)>,
) -> Vec<Planned> {
    let mut verdict: HashMap<&Path, Option<String>> = HashMap::new();
    for (path, _, hold_back) in chosen {
        verdict.insert(path, refusal(env, *hold_back, path));
    }

    // 外层 → 它里面第一个不能动的东西。
    let mut blocked: HashMap<&Path, String> = HashMap::new();
    for (path, hold_back) in all {
        let why = match verdict.get(path) {
            Some(why) => why.clone(),
            None => hold_back.filter(|h| !h.selectable()).map(hold_back_reason),
        };
        let Some(why) = why else { continue };
        for ancestor in path.ancestors().skip(1) {
            if matches!(verdict.get(ancestor), Some(None)) && !blocked.contains_key(ancestor) {
                blocked.insert(
                    ancestor,
                    format!("{REASON_CONTAINS}{}: {why}", path.display()),
                );
            }
        }
    }

    let mut ok: Vec<(PathBuf, Size)> = Vec::new();
    for (path, size, _) in chosen {
        if let Some(Some(why)) = verdict.get(path) {
            refused.push((path.to_path_buf(), why.clone()));
        } else if let Some(why) = blocked.get(path) {
            refused.push((path.to_path_buf(), why.clone()));
        } else {
            ok.push((path.to_path_buf(), *size));
        }
    }
    nest(ok)
}

/// 一项在另一项下面就并进最外层的那一项；顺序保持不变。
fn nest(items: Vec<(PathBuf, Size)>) -> Vec<Planned> {
    let paths: HashSet<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
    let top_of = |path: &Path| -> Option<PathBuf> {
        path.ancestors()
            .skip(1)
            .filter(|a| paths.contains(*a))
            .last()
            .map(Path::to_path_buf)
    };
    let mut planned: Vec<Planned> = Vec::new();
    let mut absorbed: Vec<(PathBuf, PathBuf, Size)> = Vec::new();
    for (path, size) in items {
        match top_of(&path) {
            Some(top) => absorbed.push((top, path, size)),
            None => planned.push(Planned {
                path,
                size,
                inner: Vec::new(),
            }),
        }
    }
    for (top, path, size) in absorbed {
        if let Some(outer) = planned.iter_mut().find(|p| p.path == top) {
            outer.inner.push((path, size));
        }
    }
    planned
}

pub(crate) enum Moved {
    Done,
    Refused(String),
    Failed(String),
}

/// 一次执行里"动一项"的全部步骤，带着记录和结果走。
pub(crate) struct Mover<'a> {
    env: &'a Env,
    pub(crate) ledger: &'a Ledger,
    pub(crate) record: HistoryRecord,
    pub(crate) outcome: RemovalOutcome,
    /// 真正移动之前再核一次（见 `trash_items_checked`）。
    recheck: Option<Recheck<'a>>,
}

impl<'a> Mover<'a> {
    pub(crate) fn new(
        env: &'a Env,
        ledger: &'a Ledger,
        batch: &Batch,
        outcome: RemovalOutcome,
        recheck: Option<Recheck<'a>>,
    ) -> Self {
        Mover {
            env,
            ledger,
            recheck,
            record: HistoryRecord {
                id: uuid::Uuid::new_v4().to_string(),
                at: chrono::Utc::now(),
                kind: batch.kind,
                app_name: batch.title.clone(),
                bundle_id: batch.bundle_id.clone(),
                source: batch.source,
                items: Vec::new(),
                restored: false,
            },
            outcome,
        }
    }

    /// 移外层；成功了内层随它进 `moved`（History 只记外层，放回外层内层自然回来）。
    /// 外层被拒或失败，内层重新排队，各自再过一遍全部检查，不会凭空消失。
    pub(crate) fn move_planned(&mut self, planned: Planned) -> Result<Moved, ExecuteError> {
        let moved = self.move_one(&planned.path, planned.size)?;
        if let Moved::Done = moved {
            let trashed = self
                .outcome
                .moved
                .last()
                .map(|(_, t)| t.clone())
                .expect("a move that is done was recorded");
            for (inner, _) in planned.inner {
                if let Ok(rel) = inner.strip_prefix(&planned.path) {
                    let at = trashed.join(rel);
                    self.outcome.moved.push((inner, at));
                }
            }
        } else {
            for again in nest(planned.inner) {
                self.move_planned(again)?;
            }
        }
        Ok(moved)
    }

    /// 解析 → 查保护区 → 核对身份 → （目录）查里面 → 记下意图 → 移动 → 核对搬走的是不是它 → 回填。
    /// 拒绝和失败都记在 `outcome` 里并返回；只有写不了 History 才是 `Err`。
    pub(crate) fn move_one(&mut self, path: &Path, size: Size) -> Result<Moved, ExecuteError> {
        // 先记下它现在是谁（不跟叶子链接），移完拿废纸篓里那一项对一下。
        let before = std::fs::symlink_metadata(path).ok();
        // 紧挨着移动再查一次：计划生成后路径可能被换成了链接，或者里面多了个钱包。
        let real = match preflight(self.env, path) {
            Ok(real) => real,
            Err(why) => return Ok(self.refuse(path, why)),
        };
        if before.as_ref().is_some_and(|m| m.is_dir()) {
            if let Some(why) = protect::check_descendants(&real) {
                return Ok(self.refuse(path, protected_reason(why)));
            }
        }
        if let Some(recheck) = self.recheck {
            if let Err(why) = recheck(&real) {
                return Ok(self.refuse(path, why));
            }
        }

        // 先记意图：没有记录的东西不会进废纸篓。`preflight` 已确认 `path` 就是盘上的真实位置
        // （只差 Env 根的规范化），所以记它，放回时 `validate` 按同一套规则换算。
        self.record.items.push(HistoryItem {
            original: path.to_path_buf(),
            trashed: path.to_path_buf(),
            size,
            pending: true,
            restored: false,
        });
        self.write_record()?;

        let trashed = match trash::move_to_trash(&real) {
            Ok(trashed) => trashed,
            Err(e) => {
                let why = e.to_string();
                self.forget_last();
                self.write_record()?;
                self.outcome.failed.push((path.to_path_buf(), why.clone()));
                return Ok(Moved::Failed(why));
            }
        };
        if let Some(item) = self.record.items.last_mut() {
            item.trashed = trashed.clone();
            item.pending = false;
        }

        if let Some(before) = &before {
            if !same_object(before, &trashed) {
                // 查过之后、移动之前被换掉了：搬走的不是查过的那个。放回原处；放不回就留在记录里，
                // 至少 History 里看得见它在废纸篓的哪儿。
                let why = REASON_CHANGED.to_string();
                if trash::restore(&trashed, &real).is_ok() {
                    self.forget_last();
                }
                self.write_record()?;
                self.outcome.failed.push((path.to_path_buf(), why.clone()));
                return Ok(Moved::Failed(why));
            }
        }

        self.outcome.freed += size.known().unwrap_or(0);
        self.outcome.moved.push((path.to_path_buf(), trashed));
        // 东西已经在废纸篓里了；记不下来就停，别再动下一项。
        self.write_record()?;
        Ok(Moved::Done)
    }

    fn refuse(&mut self, path: &Path, why: String) -> Moved {
        self.outcome.refused.push((path.to_path_buf(), why.clone()));
        Moved::Refused(why)
    }

    pub(crate) fn forget_last(&mut self) {
        self.record.items.pop();
    }

    /// 收尾：什么都没移成就删掉这条空记录。
    pub(crate) fn finish(self) -> RemovalOutcome {
        let Mover {
            record,
            mut outcome,
            ledger,
            ..
        } = self;
        if record.items.is_empty() {
            let _ = ledger.remove(&record.id);
            outcome.record_id = None;
        } else {
            outcome.record_id = Some(record.id);
        }
        outcome
    }

    fn write_record(&mut self) -> Result<(), ExecuteError> {
        self.ledger
            .update(&self.record)
            .map_err(|e| ExecuteError::Ledger {
                error: e.to_string(),
                partial: Box::new(self.outcome.clone()),
            })
    }
}

/// 废纸篓里那一项和移动前看到的是不是同一个文件（同一个 dev + inode）。
/// 只在 inode 号随改名不变的文件系统（APFS、HFS+）上比；exFAT / FAT 的 inode 号由目录项位置算出来，
/// 改个名就变，在那上面比只会误报。
fn same_object(before: &std::fs::Metadata, trashed: &Path) -> bool {
    if !stable_inodes(trashed) {
        return true;
    }
    match std::fs::symlink_metadata(trashed) {
        Ok(after) => after.dev() == before.dev() && after.ino() == before.ino(),
        // 看不了废纸篓里那一项：没法证明搬错了，按原结果走。
        Err(_) => true,
    }
}

fn stable_inodes(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(c_path) = path
        .parent()
        .and_then(|p| std::ffi::CString::new(p.as_os_str().as_bytes()).ok())
    else {
        return false;
    };
    // SAFETY: `stat` 是纯输出参数，全零是合法初值；`c_path` 在调用期间活着。
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    // SAFETY: 内核填的 `f_fstypename` 以 NUL 结尾。
    let name = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) };
    matches!(name.to_bytes(), b"apfs" | b"hfs")
}

/// 移动前的检查，`refusal` 和 `move_one` 共用：解析出真实位置 → 保护区 → History 放得回来 →
/// 真实位置就是扫描时那个地方。返回真实路径。
///
/// 扫描产出的路径都是"Env 的根 + 盘上的名字"，所以 `resolve(path)` 应该正好等于 History 按同一套规则
/// 换算出的位置（`history::expected_location`：根解析后接上剩下的部分）。不等说明中间某一层被换成了
/// 符号链接：比如 `~/Library/Caches` 变成指向 `~/Library/Containers` 的链接，`Caches/com.x` 实际是
/// `Containers/com.x`——移走它、History 却记着 `Caches/com.x`，就既删错了东西又放不回去。拒绝（fail closed）。
/// 两边用同一套规范化，所以 Env 的根本身不规范（`/var` → `/private/var`、大小写不同）不会误伤；
/// 叶子是符号链接时 `resolve` 不跟随，两边都是链接本身，也不会误伤。
fn preflight(env: &Env, path: &Path) -> Result<PathBuf, String> {
    let real = protect::resolve(env, path).map_err(protected_reason)?;
    if let Some(why) = protect::check(env, &real) {
        return Err(protected_reason(why));
    }
    if history::restorable(env, path).is_err() {
        return Err(REASON_NOT_RESTORABLE.to_string());
    }
    match history::expected_location(env, path) {
        Some(expected) if history::same_place(&real, &expected) => Ok(real),
        Some(_) => Err(REASON_CHANGED.to_string()),
        None => Err(REASON_NOT_RESTORABLE.to_string()),
    }
}

/// 这一项现在还能动吗？不能就说为什么。`hold_back` 里能手动选的（共用、读不了）不算拒绝。
pub(crate) fn refusal(env: &Env, hold_back: Option<&HoldBack>, path: &Path) -> Option<String> {
    if let Some(hold_back) = hold_back.filter(|h| !h.selectable()) {
        return Some(hold_back_reason(hold_back));
    }
    preflight(env, path).err()
}

/// 每种 `HoldBack` 对应的原因短语（见文件顶部的清单）。
fn hold_back_reason(hold_back: &HoldBack) -> String {
    match hold_back {
        HoldBack::Protected { why } => protected_reason(*why),
        HoldBack::NeedsAdmin => REASON_NEEDS_ADMIN.to_string(),
        HoldBack::SystemOwned => REASON_SYSTEM_OWNED.to_string(),
        HoldBack::InUse { app } => format!("{REASON_IN_USE}{app}"),
        HoldBack::Contains { path, why } => {
            format!(
                "{REASON_CONTAINS}{}: {}",
                path.display(),
                hold_back_reason(why)
            )
        }
        HoldBack::SharedWith { apps } => {
            let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
            format!("{REASON_SHARED}{}", names.join(", "))
        }
        HoldBack::Unreadable => REASON_UNREADABLE.to_string(),
    }
}

pub(crate) fn protected_reason(why: ProtectReason) -> String {
    format!(
        "{REASON_PROTECTED}{}",
        serde_json::to_value(why)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::testing::scratch;
    use std::fs;

    struct PutBack(Vec<(PathBuf, PathBuf)>);
    impl Drop for PutBack {
        fn drop(&mut self) {
            for (original, trashed) in &self.0 {
                let _ = trash::restore(trashed, original);
            }
        }
    }

    fn scratch_env() -> (tempfile::TempDir, Env) {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        fs::create_dir_all(env.user_library.join("Caches")).unwrap();
        (dir, env)
    }

    fn batch() -> Batch {
        Batch {
            kind: RecordKind::Cleanup,
            title: "test".into(),
            bundle_id: None,
            source: Source::Cli,
        }
    }

    fn item(path: PathBuf, hold_back: Option<HoldBack>) -> Removable {
        Removable {
            path,
            size: Size::Known(1),
            hold_back,
        }
    }

    #[test]
    fn moves_only_selected_and_records_a_batch() {
        let (_dir, env) = scratch_env();
        let a = env.user_library.join("Caches/a");
        let b = env.user_library.join("Caches/b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let candidates = vec![item(a.clone(), None), item(b.clone(), None)];
        let selected: HashSet<PathBuf> = [a.clone()].into_iter().collect();
        let outcome = trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger))
            .expect("moves");
        let _guard = PutBack(outcome.moved.clone());
        assert_eq!(outcome.moved.len(), 1);
        assert!(!a.exists());
        assert!(b.exists());
        let records = ledger.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, RecordKind::Cleanup);
        assert_eq!(records[0].app_name, "test");
    }

    #[test]
    fn refuses_unknown_paths_and_needs_a_ledger() {
        let (_dir, env) = scratch_env();
        let a = env.user_library.join("Caches/a");
        fs::write(&a, b"a").unwrap();
        let candidates = vec![item(a.clone(), None)];
        let stranger: HashSet<PathBuf> = [env.user_library.join("Caches/zzz")].into();
        assert!(matches!(
            trash_items(&env, &batch(), &candidates, &stranger, true, None),
            Err(ExecuteError::NotInPlan(_))
        ));
        let selected: HashSet<PathBuf> = [a.clone()].into();
        assert!(matches!(
            trash_items(&env, &batch(), &candidates, &selected, false, None),
            Err(ExecuteError::LedgerRequired)
        ));
        assert!(matches!(
            trash_items(&env, &batch(), &candidates, &HashSet::new(), true, None),
            Err(ExecuteError::NothingSelected)
        ));
        assert!(a.exists());
    }

    #[test]
    fn held_back_items_are_refused_and_nested_paths_move_once() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/outer");
        let inner = outer.join("inner");
        let busy = env.user_library.join("Caches/busy");
        fs::create_dir_all(&inner).unwrap();
        fs::write(&busy, b"x").unwrap();
        let candidates = vec![
            item(outer.clone(), None),
            item(inner.clone(), None),
            item(busy.clone(), Some(HoldBack::InUse { app: "Foo".into() })),
        ];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let outcome = trash_items(&env, &batch(), &candidates, &selected, true, None).unwrap();
        assert_eq!(
            outcome.moved,
            vec![
                (outer.clone(), outer.clone()),
                (inner.clone(), inner.clone())
            ],
            "the inner one goes with the outer one and is listed, not moved twice"
        );
        assert_eq!(
            outcome.freed, 1,
            "the outer size already covers the inner one"
        );
        assert_eq!(outcome.refused.len(), 1);
        assert_eq!(outcome.refused[0].0, busy);
    }

    fn ledger(env: &Env) -> Ledger {
        Ledger::at(env.data_dir.join("history.jsonl"))
    }

    #[test]
    fn an_outer_folder_holding_a_refused_inner_item_is_refused_too() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/com.foo");
        let busy = outer.join("busy");
        let admin = outer.join("admin");
        fs::create_dir_all(&busy).unwrap();
        fs::create_dir_all(&admin).unwrap();
        let ledger = ledger(&env);

        // 内层被选中但不可选。
        let candidates = vec![
            item(outer.clone(), None),
            item(busy.clone(), Some(HoldBack::InUse { app: "Foo".into() })),
        ];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![
                (
                    outer.clone(),
                    format!("contains {}: in use by Foo", busy.display())
                ),
                (busy.clone(), "in use by Foo".to_string()),
            ]
        );
        assert!(outer.exists() && busy.exists());
        assert!(ledger.list().unwrap().is_empty());

        // 内层没被选中，但扫描说它不能动：外层一样不能走。
        let candidates = vec![
            item(outer.clone(), None),
            item(admin.clone(), Some(HoldBack::NeedsAdmin)),
        ];
        let selected: HashSet<PathBuf> = [outer.clone()].into();
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(
                outer.clone(),
                format!("contains {}: needs an administrator", admin.display())
            )]
        );
        assert!(admin.exists());

        // 共用（可以手动选）的内层不挡外层。
        let candidates = vec![
            item(outer.clone(), None),
            item(admin.clone(), Some(HoldBack::SharedWith { apps: vec![] })),
        ];
        let outcome = trash_items(&env, &batch(), &candidates, &selected, true, None).unwrap();
        assert_eq!(outcome.moved, vec![(outer.clone(), outer.clone())]);
    }

    #[test]
    fn when_the_outer_folder_moves_the_inner_item_is_listed_and_history_has_only_the_outer() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/com.foo");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("f"), b"x").unwrap();
        let ledger = ledger(&env);
        let candidates = vec![item(inner.clone(), None), item(outer.clone(), None)];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.refused.is_empty() && outcome.failed.is_empty());
        assert_eq!(outcome.moved.len(), 2);
        assert_eq!(outcome.moved[0].0, outer);
        let trashed_outer = outcome.moved[0].1.clone();
        assert_eq!(
            outcome.moved[1],
            (inner.clone(), trashed_outer.join("inner"))
        );
        assert!(trashed_outer.join("inner/f").exists());
        assert!(!outer.exists());
        let records = ledger.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].items.len(), 1, "History only has the outer one");
        assert_eq!(records[0].items[0].original, outer);
        assert_eq!(records[0].items[0].trashed, trashed_outer);
    }

    #[test]
    fn when_the_outer_folder_cannot_move_the_inner_item_is_tried_on_its_own() {
        let (_dir, env) = scratch_env();
        let ledger = ledger(&env);

        // 外层在移动前的复查里被拒（里面多了个照片库）：内层自己再走一遍。
        let outer = env.user_library.join("Caches/com.foo");
        let inner = outer.join("cache");
        fs::create_dir_all(&inner).unwrap();
        fs::create_dir_all(outer.join("Old.photoslibrary")).unwrap();
        let candidates = vec![item(outer.clone(), None), item(inner.clone(), None)];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert_eq!(
            outcome.refused,
            vec![(outer.clone(), "protected: photo_library".to_string())]
        );
        assert_eq!(outcome.moved.len(), 1);
        assert_eq!(outcome.moved[0].0, inner);
        assert!(outer.join("Old.photoslibrary").is_dir());

        // 外层移动失败（父目录只读）：内层不会凭空消失，自己移走。
        let vendor = env.user_library.join("Caches/com.bar");
        let outer = vendor.join("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        struct Writable(PathBuf);
        impl Drop for Writable {
            fn drop(&mut self) {
                let _ = fs::set_permissions(
                    &self.0,
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                );
            }
        }
        fs::set_permissions(&vendor, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let _writable = Writable(vendor.clone());
        let candidates = vec![item(outer.clone(), None), item(inner.clone(), None)];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert_eq!(outcome.failed.len(), 1, "{outcome:?}");
        assert_eq!(outcome.failed[0].0, outer);
        assert_eq!(outcome.moved.len(), 1, "{outcome:?}");
        assert_eq!(outcome.moved[0].0, inner);
        assert!(outer.is_dir());
        let record = ledger
            .get(outcome.record_id.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(record.items.len(), 1);
        assert_eq!(record.items[0].original, inner);
    }

    #[test]
    fn a_parent_swapped_for_a_symlink_after_the_scan_is_refused_and_the_target_untouched() {
        // SEC-H1：扫描后 `~/Library/Caches` 被换成指向（假的）`~/Library/Containers` 的链接，
        // `Caches/com.x` 实际是 `Containers/com.x`。以前会照移、History 却记着 `Caches/com.x`。
        let (_dir, env) = scratch_env();
        let caches = env.user_library.join("Caches");
        let scanned = caches.join("com.x");
        fs::create_dir_all(&scanned).unwrap();
        let candidates = vec![item(scanned.clone(), None)];
        let selected: HashSet<PathBuf> = [scanned.clone()].into();

        let containers = env.user_library.join("Containers");
        let victim = containers.join("com.x");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("Data"), b"precious").unwrap();
        fs::rename(&caches, env.user_library.join("Caches.old")).unwrap();
        std::os::unix::fs::symlink(&containers, &caches).unwrap();

        let ledger = ledger(&env);
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(scanned.clone(), "path changed since scan".to_string())]
        );
        assert_eq!(fs::read(victim.join("Data")).unwrap(), b"precious");
        assert!(ledger.list().unwrap().is_empty());

        // 移动那一刻的复查也会挡下（扫描后的预检过了、之后才被换掉的情形）。
        let mut mover = Mover::new(&env, &ledger, &batch(), RemovalOutcome::default(), None);
        assert!(matches!(
            mover.move_one(&scanned, Size::Known(1)).unwrap(),
            Moved::Refused(ref why) if why == "path changed since scan"
        ));
        assert!(victim.join("Data").exists());
        assert!(mover.finish().record_id.is_none());
    }

    #[test]
    fn a_photo_library_somewhere_inside_is_refused() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/com.foo");
        let library = outer.join("a/b/c/Pictures.photoslibrary");
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join("database"), b"db").unwrap();
        let candidates = vec![item(outer.clone(), None)];
        let selected: HashSet<PathBuf> = [outer.clone()].into();
        let ledger = ledger(&env);
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(outer.clone(), "protected: photo_library".to_string())]
        );
        assert!(library.join("database").exists());
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn a_dry_run_says_the_same_as_a_real_run() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/com.foo");
        let library = outer.join("a/b/c/Pictures.photoslibrary");
        let safe = outer.join("proj");
        fs::create_dir_all(&library).unwrap();
        fs::create_dir_all(&safe).unwrap();
        fs::write(safe.join("data"), b"x").unwrap();
        let candidates = vec![item(outer.clone(), None), item(safe.clone(), None)];
        let selected: HashSet<PathBuf> = [outer.clone(), safe.clone()].into();

        let dry = trash_items(&env, &batch(), &candidates, &selected, true, None).unwrap();
        assert!(dry.dry_run);
        assert_eq!(
            dry.refused,
            vec![(outer.clone(), "protected: photo_library".to_string())]
        );
        let dry_paths: Vec<PathBuf> = dry.moved.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(dry_paths, vec![safe.clone()], "预演只说内层会走");

        let ledger = ledger(&env);
        let real =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(real.moved.clone());
        assert_eq!(real.refused, dry.refused);
        let real_paths: Vec<PathBuf> = real.moved.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(real_paths, dry_paths, "真跑走的就是预演说会走的");
    }

    #[test]
    fn a_refused_outer_does_not_double_count_its_inner_items() {
        let (_dir, env) = scratch_env();
        let outer = env.user_library.join("Caches/com.outer");
        let sub = outer.join("sub");
        let deeper = sub.join("deeper");
        fs::create_dir_all(&deeper).unwrap();
        fs::write(deeper.join("x"), b"x").unwrap();
        fs::create_dir_all(outer.join("Blocked.photoslibrary")).unwrap();
        let sized = |path: &Path, bytes: u64| Removable {
            path: path.to_path_buf(),
            size: Size::Known(bytes),
            hold_back: None,
        };
        let candidates = vec![sized(&outer, 100), sized(&sub, 60), sized(&deeper, 20)];
        let selected: HashSet<PathBuf> = [outer.clone(), sub.clone(), deeper.clone()].into();

        let dry = trash_items(&env, &batch(), &candidates, &selected, true, None).unwrap();
        let ledger = ledger(&env);
        let real =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(real.moved.clone());
        assert_eq!(dry.refused, real.refused);
        assert_eq!(
            dry.freed, real.freed,
            "预演算出的能释放大小要和真跑一样：内层只算最外层那一项"
        );
        assert_eq!(dry.freed, 60);
    }

    #[test]
    fn places_history_cannot_put_back_are_refused() {
        let (dir, env) = scratch_env();
        let stray = dir.path().join("elsewhere/file");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        fs::write(&stray, b"x").unwrap();
        // `/Volumes/Macintosh HD` 是指向 `/` 的链接：跟着它就能绕到启动盘上任何地方。
        let volumes = dir.path().join("Volumes");
        fs::create_dir_all(&volumes).unwrap();
        std::os::unix::fs::symlink(dir.path(), volumes.join("Boot")).unwrap();
        let via_boot = volumes.join("Boot/elsewhere/file");

        let candidates = vec![item(stray.clone(), None), item(via_boot.clone(), None)];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let ledger = ledger(&env);
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.moved.is_empty());
        assert_eq!(outcome.refused.len(), 2);
        assert!(outcome
            .refused
            .iter()
            .all(|(_, why)| why == "outside the places History can put back"));
        assert!(stray.exists());
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn shared_folders_and_external_volumes_can_be_moved_and_put_back() {
        let (dir, env) = scratch_env();
        let shared = dir.path().join("Users/Shared/com.foo");
        let external = dir.path().join("Volumes/Ext/Caches/com.foo");
        for p in [&shared, &external] {
            fs::create_dir_all(p).unwrap();
            fs::write(p.join("f"), b"x").unwrap();
        }
        let candidates = vec![item(shared.clone(), None), item(external.clone(), None)];
        let selected: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let ledger = ledger(&env);
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        let _guard = PutBack(outcome.moved.clone());
        assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
        assert_eq!(outcome.moved.len(), 2);
        let restored = ledger
            .restore(&env, outcome.record_id.as_ref().unwrap())
            .unwrap();
        assert_eq!(restored.restored, vec![shared.clone(), external.clone()]);
        assert!(shared.join("f").exists() && external.join("f").exists());
    }

    #[test]
    fn every_hold_back_maps_to_a_stable_reason() {
        let inner = HoldBack::Protected {
            why: ProtectReason::PhotoLibrary,
        };
        let cases = [
            (inner.clone(), "protected: photo_library".to_string()),
            (HoldBack::NeedsAdmin, "needs an administrator".to_string()),
            (HoldBack::SystemOwned, "belongs to macOS".to_string()),
            (
                HoldBack::InUse { app: "Foo".into() },
                "in use by Foo".to_string(),
            ),
            (
                HoldBack::Contains {
                    path: "/x/y".into(),
                    why: Box::new(inner),
                },
                "contains /x/y: protected: photo_library".to_string(),
            ),
            (HoldBack::Unreadable, "unreadable".to_string()),
        ];
        let (_dir, env) = scratch_env();
        let path = env.user_library.join("Caches/com.foo");
        for (hold_back, expected) in cases {
            assert_eq!(hold_back_reason(&hold_back), expected);
            if !hold_back.selectable() {
                assert_eq!(refusal(&env, Some(&hold_back), &path), Some(expected));
            }
        }
        assert_eq!(
            refusal(&env, Some(&HoldBack::Unreadable), &path),
            None,
            "selectable hold-backs are not refusals"
        );
    }

    #[test]
    fn protected_paths_are_refused_even_when_selected() {
        let (_dir, env) = scratch_env();
        let keychains = env.user_library.join("Keychains");
        fs::create_dir_all(&keychains).unwrap();
        let candidates = vec![item(keychains.clone(), None)];
        let selected: HashSet<PathBuf> = [keychains.clone()].into();
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let outcome =
            trash_items(&env, &batch(), &candidates, &selected, false, Some(&ledger)).unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(outcome.refused.len(), 1);
        assert!(keychains.exists());
        assert!(outcome.record_id.is_none());
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn a_different_file_is_not_the_same_object() {
        let (_dir, env) = scratch_env();
        let a = env.user_library.join("Caches/a");
        let b = env.user_library.join("Caches/b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let before = fs::symlink_metadata(&a).unwrap();
        assert!(same_object(&before, &a));
        assert!(!same_object(&before, &b));
    }
}
