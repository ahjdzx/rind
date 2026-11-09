//! History：Rind 移走过什么，以及把它放回去。JSON Lines，一行一条记录。
//!
//! 读写都拿同一把文件锁（`history.lock` 上的 `flock`），所以 CLI 和 GUI、应用页和历史页同时动它也不会
//! 互相覆盖；重写走"唯一的临时文件 + fsync + rename"。放回之前每一项都要核对：东西真的在废纸篓里、
//! 原位置是个正常的地方——History 文件是用户可改的，不能让它把任意文件挪到任意位置。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::trash::{self, TrashError};
use crate::{protect, Env, HistoryItem, HistoryRecord, RestoreOutcome};

#[derive(Clone, Debug)]
pub struct Ledger {
    pub path: PathBuf,
}

/// 文件里的一行：能读懂的记录，或者读不懂的原文（重写时原样保留，不丢任何东西）。
enum Line {
    Record(HistoryRecord),
    Raw(String),
}

/// 拿着锁的时候文件不会被别人改；放掉（drop）就解锁。
struct Lock(File);

impl Lock {
    fn take(path: &Path, exclusive: bool) -> anyhow::Result<Lock> {
        let lock_path = path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        // SAFETY: fd 来自上面打开的、还活着的 File。
        if unsafe { libc::flock(file.as_raw_fd(), op) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", lock_path.display()));
        }
        Ok(Lock(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: fd 还活着；解锁失败没有可做的事，进程退出时内核也会放掉。
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Ledger {
    /// `env.data_dir/history.jsonl`，目录不存在就建。
    pub fn open(env: &Env) -> anyhow::Result<Ledger> {
        fs::create_dir_all(&env.data_dir)
            .with_context(|| format!("creating {}", env.data_dir.display()))?;
        Ok(Ledger::at(env.data_dir.join("history.jsonl")))
    }

    pub fn at(path: PathBuf) -> Ledger {
        Ledger { path }
    }

    /// 追加一条。
    pub fn append(&self, record: &HistoryRecord) -> anyhow::Result<()> {
        let _lock = Lock::take(&self.path, true)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut line = serde_json::to_string(record).context("encoding history record")?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        file.sync_data().ok();
        Ok(())
    }

    /// 按 id 整条替换（比如标记 restored）。记录不存在就追加。
    pub fn update(&self, record: &HistoryRecord) -> anyhow::Result<()> {
        let _lock = Lock::take(&self.path, true)?;
        let mut lines = self.read_lines()?;
        let mut replaced = false;
        for line in &mut lines {
            if let Line::Record(existing) = line {
                if existing.id == record.id {
                    *existing = record.clone();
                    replaced = true;
                    break;
                }
            }
        }
        if !replaced {
            lines.push(Line::Record(record.clone()));
        }
        self.rewrite(&lines)
    }

    /// 删掉一条（比如一次什么都没移成的尝试）。不存在也算成功。
    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _lock = Lock::take(&self.path, true)?;
        let lines: Vec<Line> = self
            .read_lines()?
            .into_iter()
            .filter(|line| !matches!(line, Line::Record(r) if r.id == id))
            .collect();
        self.rewrite(&lines)
    }

    /// 全部记录，新的在前。
    pub fn list(&self) -> anyhow::Result<Vec<HistoryRecord>> {
        let _lock = Lock::take(&self.path, false)?;
        let mut records: Vec<HistoryRecord> = self
            .read_lines()?
            .into_iter()
            .filter_map(|line| match line {
                Line::Record(r) => Some(r),
                Line::Raw(_) => None,
            })
            .collect();
        records.reverse();
        Ok(records)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<HistoryRecord>> {
        Ok(self.list()?.into_iter().find(|r| r.id == id))
    }

    /// 把一条记录里的每一项放回去（`trash::restore`），每放回一项就记一项。
    /// 废纸篓里已经没有的、原位置被占的、记录本身不对劲的（不在废纸篓里、原位置在保护区或奇怪的地方），
    /// 都进 `skipped` 并说明。没有东西再留在废纸篓里时记录标 `restored`；还有东西卡着（原位置被占、
    /// 没权限）就不标，好让人处理后再试。
    pub fn restore(&self, env: &Env, id: &str) -> anyhow::Result<RestoreOutcome> {
        let mut record = self
            .get(id)?
            .with_context(|| format!("no history record {id}"))?;
        let mut outcome = RestoreOutcome::default();
        let mut still_in_trash = false;
        let mut changed = false;

        for item in &mut record.items {
            if item.restored {
                outcome
                    .skipped
                    .push((item.original.clone(), "already restored".to_string()));
                continue;
            }
            if item.pending {
                outcome
                    .skipped
                    .push((item.original.clone(), "was never moved".to_string()));
                continue;
            }
            if let Err(why) = validate(env, item) {
                still_in_trash = true;
                outcome.skipped.push((item.original.clone(), why));
                continue;
            }
            match trash::restore(&item.trashed, &item.original) {
                Ok(()) => {
                    item.restored = true;
                    changed = true;
                    outcome.restored.push(item.original.clone());
                }
                Err(TrashError::NotFound(_)) => {
                    outcome
                        .skipped
                        .push((item.original.clone(), "no longer in the Trash".to_string()));
                }
                Err(err) => {
                    still_in_trash = true;
                    outcome
                        .skipped
                        .push((item.original.clone(), err.to_string()));
                }
            }
        }
        if !still_in_trash && !record.restored {
            record.restored = true;
            changed = true;
        }
        if changed {
            self.update(&record)?;
        }
        Ok(outcome)
    }

    fn read_lines(&self) -> anyhow::Result<Vec<Line>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        Ok(text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| match serde_json::from_str::<HistoryRecord>(line) {
                Ok(record) => Line::Record(record),
                Err(_) => Line::Raw(line.to_string()),
            })
            .collect())
    }

    /// 先写一个名字唯一的临时文件、落盘，再改名顶替，中途断了也不会留下半个文件。调用方已持锁。
    fn rewrite(&self, lines: &[Line]) -> anyhow::Result<()> {
        let parent = self
            .path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
        let mut text = String::new();
        for line in lines {
            match line {
                Line::Record(record) => text.push_str(&serde_json::to_string(record)?),
                Line::Raw(raw) => text.push_str(raw),
            }
            text.push('\n');
        }
        let mut temp = tempfile::Builder::new()
            .prefix(".history.")
            .suffix(".tmp")
            .tempfile_in(&parent)
            .with_context(|| format!("creating a temporary file in {}", parent.display()))?;
        temp.write_all(text.as_bytes()).context("writing history")?;
        temp.as_file().sync_all().context("syncing history")?;
        temp.persist(&self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

/// 这一项能不能放：废纸篓里的那份真的在废纸篓里，原位置是个绝对的、不在保护区的、在我们认识的地方的路径，
/// 而且中间没有符号链接把它引到别处。
fn validate(env: &Env, item: &HistoryItem) -> Result<(), String> {
    let trashed_parent = item
        .trashed
        .parent()
        .ok_or("the trashed path has no parent")?;
    let trashed_parent = fs::canonicalize(trashed_parent)
        .map_err(|e| format!("{}: {e}", trashed_parent.display()))?;
    if !is_a_trash(env, &trashed_parent) {
        return Err(format!("{} is not in the Trash", item.trashed.display()));
    }
    let expected = expected_or_err(env, &item.original)?;
    let real = protect::resolve(env, &item.original).map_err(|_| {
        format!(
            "{} leads somewhere else on disk (it does not resolve)",
            item.original.display()
        )
    })?;
    if !same_place(&real, &expected) {
        return Err(format!(
            "{} leads somewhere else on disk ({})",
            item.original.display(),
            real.display()
        ));
    }
    if let Some(why) = protect::check(env, &real) {
        return Err(format!(
            "{} is protected ({why:?})",
            item.original.display()
        ));
    }
    Ok(())
}

/// `~/.Trash`（这个 Env 的，和真实用户的，因为 macOS 只会往后者放）或者外置卷的 `.Trashes/<uid>`。
fn is_a_trash(env: &Env, dir: &Path) -> bool {
    let mut trashes = vec![env.home.join(".Trash")];
    if let Some(real_home) = dirs::home_dir() {
        trashes.push(real_home.join(".Trash"));
    }
    for t in trashes {
        if let Ok(t) = fs::canonicalize(&t) {
            if dir.starts_with(&t) {
                return true;
            }
        }
    }
    // `/Volumes/<卷>/.Trashes/<uid>/...`
    let parts: Vec<String> = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    parts.len() >= 5
        && parts[1] == "Volumes"
        && parts[3] == ".Trashes"
        && parts[4] == unsafe { libc::getuid() }.to_string()
}

/// History 能不能把 `path` 放回去：和放回时 `validate` 用同一套位置规则（`expected_location`）。
/// 移动前就要问：放不回来的东西不该进废纸篓。
pub fn restorable(env: &Env, path: &Path) -> Result<(), String> {
    expected_or_err(env, path).map(|_| ())
}

fn expected_or_err(env: &Env, original: &Path) -> Result<PathBuf, String> {
    if !original.is_absolute()
        || original
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "{} is not a plain absolute path",
            original.display()
        ));
    }
    expected_location(env, original)
        .ok_or_else(|| format!("{} is outside the places Rind knows", original.display()))
}

/// 把 `original` 通过它所在的根换算成盘上应有的位置：根解析后再接上剩下的部分。不在任何已知根下 → None。
///
/// 认的根：家目录、`/Library`、收据目录、App 目录、Homebrew 前缀、`/Users/Shared`，以及外置卷 `/Volumes/<卷名>`。
/// - `/Users/Shared` 本来就人人可写，往里放回东西不会给谁多出权限。
/// - `/Volumes/<卷名>` 只认真正挂在那儿的目录：`/Volumes/Macintosh HD` 是指向 `/` 的符号链接，跟着它走
///   就能把东西"放回"启动盘上任何地方，所以卷根是符号链接（或不是目录）一律不认；卷根本身也不算。
///
/// 保护区和"父目录解析后没逃出这个根"由调用方（`validate`、`remove`）照旧再查。
pub(crate) fn expected_location(env: &Env, original: &Path) -> Option<PathBuf> {
    let system_root = env.system_library.parent().unwrap_or(Path::new("/"));
    let shared = system_root.join("Users/Shared");
    let mut roots: Vec<&Path> = vec![&env.home, &env.system_library, &env.receipts_dir, &shared];
    roots.extend(env.app_dirs.iter().map(PathBuf::as_path));
    if let Some(prefix) = &env.homebrew_prefix {
        roots.push(prefix);
    }
    // 最长的根优先，免得 `~` 抢了 `~/Library` 的活（结果一样，但说清楚）。
    roots.sort_by_key(|r| std::cmp::Reverse(r.as_os_str().len()));
    for root in roots {
        if let Ok(rest) = original.strip_prefix(root) {
            let real_root = protect::resolve(env, root).ok()?;
            return Some(real_root.join(rest));
        }
    }

    let volumes = system_root.join("Volumes");
    let rest = original.strip_prefix(&volumes).ok()?;
    let mut parts = rest.components();
    let Some(std::path::Component::Normal(name)) = parts.next() else {
        return None;
    };
    let inside = parts.as_path();
    if inside.as_os_str().is_empty() {
        return None;
    }
    let volume = volumes.join(name);
    let is_mount_dir = fs::symlink_metadata(&volume).is_ok_and(|m| m.is_dir());
    if !is_mount_dir {
        return None;
    }
    let real_volume = protect::resolve(env, &volume).ok()?;
    let real_volumes = protect::resolve(env, &volumes).ok()?;
    if !same_place(real_volume.parent()?, &real_volumes) {
        return None;
    }
    Some(real_volume.join(inside))
}

/// 两条已解析的路径是不是同一个地方（APFS 不区分大小写）。
pub(crate) fn same_place(a: &Path, b: &Path) -> bool {
    let a: Vec<String> = a
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let b: Vec<String> = b
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::testing::{scratch, PutBack};
    use crate::{RecordKind, Size, Source};

    fn record(id: &str, items: Vec<HistoryItem>) -> HistoryRecord {
        HistoryRecord {
            id: id.to_string(),
            at: chrono::Utc::now(),
            kind: RecordKind::Uninstall,
            app_name: "Foo".to_string(),
            bundle_id: Some("com.foo.bar".to_string()),
            source: Source::Cli,
            items,
            restored: false,
        }
    }

    fn item(original: &Path, trashed: &Path) -> HistoryItem {
        HistoryItem {
            original: original.to_path_buf(),
            trashed: trashed.to_path_buf(),
            size: Size::Known(5),
            pending: false,
            restored: false,
        }
    }

    /// 假的家目录放在真实家目录下面，这样废纸篓在同一个卷上，而 Env 的根都在临时目录里。
    fn scratch_env() -> (tempfile::TempDir, Env) {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        fs::create_dir_all(&env.user_library).unwrap();
        (dir, env)
    }

    #[test]
    fn append_list_get_update_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("deep/history.jsonl"));
        assert!(
            ledger.list().unwrap().is_empty(),
            "no file yet is an empty history"
        );

        ledger.append(&record("a", vec![])).unwrap();
        ledger.append(&record("b", vec![])).unwrap();
        let listed = ledger.list().unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"],
            "newest first"
        );

        let mut a = ledger.get("a").unwrap().unwrap();
        a.restored = true;
        a.app_name = "Renamed".to_string();
        ledger.update(&a).unwrap();
        let a_again = ledger.get("a").unwrap().unwrap();
        assert!(a_again.restored);
        assert_eq!(a_again.app_name, "Renamed");
        assert_eq!(
            ledger.list().unwrap().len(),
            2,
            "update replaces, never duplicates"
        );
        assert!(ledger.get("zzz").unwrap().is_none());

        ledger.remove("a").unwrap();
        assert_eq!(ledger.list().unwrap().len(), 1);
        ledger.remove("never").unwrap();
        assert!(!dir.path().join("deep").read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[test]
    fn a_broken_line_is_kept_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));
        ledger.append(&record("a", vec![])).unwrap();
        let mut file = OpenOptions::new().append(true).open(&ledger.path).unwrap();
        file.write_all(b"{\"id\": \"half\n").unwrap();
        drop(file);

        assert_eq!(ledger.list().unwrap().len(), 1);
        ledger.append(&record("b", vec![])).unwrap();
        ledger.update(&record("a", vec![])).unwrap();
        let text = fs::read_to_string(&ledger.path).unwrap();
        assert!(
            text.contains("{\"id\": \"half"),
            "rewriting keeps what it could not read"
        );
        assert_eq!(ledger.list().unwrap().len(), 2);
    }

    #[test]
    fn concurrent_appends_and_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));
        ledger.append(&record("seed", vec![])).unwrap();
        let appenders = 4;
        let per_thread = 60;
        std::thread::scope(|scope| {
            for t in 0..appenders {
                let ledger = ledger.clone();
                scope.spawn(move || {
                    for i in 0..per_thread {
                        ledger
                            .append(&record(&format!("a{t}-{i}"), vec![]))
                            .unwrap();
                    }
                });
            }
            for t in 0..3 {
                let ledger = ledger.clone();
                scope.spawn(move || {
                    for i in 0..per_thread {
                        let mut seed = ledger.get("seed").unwrap().unwrap();
                        seed.app_name = format!("u{t}-{i}");
                        ledger.update(&seed).unwrap();
                    }
                });
            }
        });
        let all = ledger.list().unwrap();
        assert_eq!(
            all.len(),
            1 + appenders * per_thread,
            "every append survived every rewrite"
        );
        assert!(all
            .iter()
            .any(|r| r.id == "seed" && r.app_name.starts_with('u')));
        assert!(!dir.path().read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[test]
    fn restore_puts_items_back_and_marks_each() {
        let (dir, env) = scratch_env();
        let mut guard = PutBack::new();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));

        let one = env.user_library.join("Caches/one.txt");
        let two = env.user_library.join("Caches/two");
        fs::create_dir_all(two.join("x")).unwrap();
        fs::write(&one, b"one").unwrap();
        let one_trashed = guard.trash(&one).unwrap();
        let two_trashed = guard.trash(&two).unwrap();
        ledger
            .append(&record(
                "r",
                vec![item(&one, &one_trashed), item(&two, &two_trashed)],
            ))
            .unwrap();

        let outcome = ledger.restore(&env, "r").unwrap();
        assert_eq!(outcome.restored, vec![one.clone(), two.clone()]);
        assert!(outcome.skipped.is_empty());
        assert_eq!(fs::read(&one).unwrap(), b"one");
        assert!(two.join("x").is_dir());
        let stored = ledger.get("r").unwrap().unwrap();
        assert!(stored.restored);
        assert!(stored.items.iter().all(|i| i.restored));

        let again = ledger.restore(&env, "r").unwrap();
        assert!(again.restored.is_empty());
        assert_eq!(again.skipped.len(), 2);
        assert!(again
            .skipped
            .iter()
            .all(|(_, why)| why == "already restored"));
    }

    #[test]
    fn restore_reports_what_is_gone_and_what_is_blocked_and_retries_only_the_rest() {
        let (dir, env) = scratch_env();
        let mut guard = PutBack::new();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));

        let gone = env.user_library.join("Caches/gone.txt");
        let blocked = env.user_library.join("Caches/blocked.txt");
        let fine = env.user_library.join("Caches/fine.txt");
        fs::create_dir_all(env.user_library.join("Caches")).unwrap();
        fs::write(&blocked, b"old").unwrap();
        fs::write(&fine, b"fine").unwrap();
        let blocked_trashed = guard.trash(&blocked).unwrap();
        let fine_trashed = guard.trash(&fine).unwrap();
        fs::write(&blocked, b"new").unwrap();
        let gone_trashed = dirs::home_dir().unwrap().join(".Trash/rind-never-existed");
        ledger
            .append(&record(
                "r",
                vec![
                    item(&gone, &gone_trashed),
                    item(&blocked, &blocked_trashed),
                    item(&fine, &fine_trashed),
                ],
            ))
            .unwrap();

        let outcome = ledger.restore(&env, "r").unwrap();
        assert_eq!(outcome.restored, vec![fine.clone()]);
        assert_eq!(outcome.skipped.len(), 2);
        assert_eq!(outcome.skipped[0].0, gone);
        assert_eq!(outcome.skipped[0].1, "no longer in the Trash");
        assert!(
            outcome.skipped[1].1.starts_with("destination exists"),
            "{}",
            outcome.skipped[1].1
        );
        let stored = ledger.get("r").unwrap().unwrap();
        assert!(
            !stored.restored,
            "something is still in the Trash, so it can be retried"
        );
        assert!(stored.items[2].restored, "but what came back is remembered");

        // 让开原位置再试：只剩 blocked 要放，fine 不再被碰。
        fs::remove_file(&blocked).unwrap();
        let outcome = ledger.restore(&env, "r").unwrap();
        assert_eq!(outcome.restored, vec![blocked.clone()]);
        assert_eq!(fs::read(&blocked).unwrap(), b"old");
        assert!(outcome
            .skipped
            .iter()
            .any(|(p, why)| p == &fine && why == "already restored"));
        assert!(ledger.get("r").unwrap().unwrap().restored);
    }

    #[test]
    fn restore_refuses_a_record_that_points_outside_the_trash_or_into_protected_places() {
        // History 是用户可改的文件：不能让一条伪造的记录把任意文件挪到任意位置。
        let (dir, env) = scratch_env();
        let mut guard = PutBack::new();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));

        let secret = env.user_library.join("Caches/secret-key");
        fs::create_dir_all(env.user_library.join("Caches")).unwrap();
        fs::write(&secret, b"ssh").unwrap();
        let stolen = env.home.join("Public/stolen");
        let ok = env.user_library.join("Caches/ok.txt");
        fs::write(&ok, b"ok").unwrap();
        let ok_trashed = guard.trash(&ok).unwrap();
        let keychain = env.user_library.join("Keychains/login.keychain-db");
        let elsewhere = Path::new("/tmp/rind-elsewhere");
        ledger
            .append(&record(
                "r",
                vec![
                    item(&stolen, &secret),       // "trashed" 根本不在废纸篓里
                    item(&keychain, &ok_trashed), // 原位置在保护区
                    item(elsewhere, &ok_trashed), // 原位置不在认识的地方
                    item(&ok, &ok_trashed),       // 这条是对的
                ],
            ))
            .unwrap();

        let outcome = ledger.restore(&env, "r").unwrap();
        assert_eq!(outcome.restored, vec![ok.clone()]);
        assert_eq!(outcome.skipped.len(), 3, "{:?}", outcome.skipped);
        assert!(
            outcome.skipped[0].1.contains("not in the Trash"),
            "{}",
            outcome.skipped[0].1
        );
        assert!(
            outcome.skipped[1].1.contains("protected"),
            "{}",
            outcome.skipped[1].1
        );
        assert!(
            outcome.skipped[2].1.contains("outside"),
            "{}",
            outcome.skipped[2].1
        );
        assert_eq!(
            fs::read(&secret).unwrap(),
            b"ssh",
            "the file that was never in the Trash stayed put"
        );
        assert!(!stolen.exists());
        assert!(!keychain.exists());
    }

    #[test]
    fn restore_refuses_an_original_whose_parent_became_a_symlink() {
        let (dir, env) = scratch_env();
        let mut guard = PutBack::new();
        let ledger = Ledger::at(dir.path().join("history.jsonl"));

        let caches = env.user_library.join("Caches");
        fs::create_dir_all(&caches).unwrap();
        let file = caches.join("com.foo/data");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"d").unwrap();
        let trashed = guard.trash(&file).unwrap();
        ledger
            .append(&record("r", vec![item(&file, &trashed)]))
            .unwrap();

        // 放回之前，`com.foo` 被换成了指向别处的符号链接。
        fs::remove_dir_all(caches.join("com.foo")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, caches.join("com.foo")).unwrap();

        let outcome = ledger.restore(&env, "r").unwrap();
        assert!(outcome.restored.is_empty());
        assert_eq!(outcome.skipped.len(), 1);
        assert!(
            outcome.skipped[0].1.contains("somewhere else"),
            "{}",
            outcome.skipped[0].1
        );
        assert!(
            !elsewhere.join("data").exists(),
            "nothing landed behind the link"
        );
        assert!(trashed.exists());
    }

    #[test]
    fn pending_items_are_skipped_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let ledger = Ledger::at(dir.path().join("history.jsonl"));
        let original = env.user_library.join("Caches/x");
        let mut pending = item(&original, &original);
        pending.pending = true;
        ledger.append(&record("r", vec![pending])).unwrap();
        let outcome = ledger.restore(&env, "r").unwrap();
        assert_eq!(
            outcome.skipped,
            vec![(original, "was never moved".to_string())]
        );
        assert!(
            ledger.get("r").unwrap().unwrap().restored,
            "nothing is left in the Trash for it"
        );
    }

    #[test]
    fn restoring_an_unknown_record_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let ledger = Ledger::at(dir.path().join("history.jsonl"));
        assert!(ledger.restore(&env, "nope").is_err());
    }
}
