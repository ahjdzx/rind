//! 找出已安装的 App，并收集匹配残留所需的全部证据来源。

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::{AppSource, Env, InstalledApp, Size};

/// 扫描 `env.app_dirs` 里的每个 `.app`（并行），按名字排序。
/// Apple 自带的 App（`com.apple.*`，或在 `env.system_app_dirs` 下）也返回，但 `is_system = true`。
pub fn scan_apps(env: &Env) -> Vec<InstalledApp> {
    let mut bundles: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in &env.app_dirs {
        for entry in list_dir(dir) {
            if is_app_bundle(&entry) {
                bundles.insert(entry);
            } else if entry.is_dir() && !env.system_app_dirs.contains(&entry) {
                // `/Applications/Adobe/...` 这样的一层子目录。
                bundles.extend(list_dir(&entry).into_iter().filter(|p| is_app_bundle(p)));
            }
        }
    }
    for dir in &env.system_app_dirs {
        bundles.extend(list_dir(dir).into_iter().filter(|p| is_app_bundle(p)));
    }
    let caskroom = caskroom_apps(env);
    let mut apps: Vec<InstalledApp> = bundles
        .into_par_iter()
        .filter_map(|path| read_app_with(env, &path, &caskroom))
        .collect();
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    apps
}

/// 读一个 `.app`：Info.plist、内嵌组件 id、Team ID、App Group、来源、大小（不度量，`Size::Unknown`，由上层按需度量）。
pub fn read_app(env: &Env, path: &Path) -> Option<InstalledApp> {
    read_app_with(env, path, &caskroom_apps(env))
}

/// 按名字 / bundle id / 路径（不分大小写、可只给一部分；路径里的 `~` 会展开）找 App。
/// 有完全相等的可卸载 App 就只返回它们；完全相等的只有系统 App 时，把包含查询的也一起给，
/// 让调用方决定；否则返回包含查询的。
pub fn find_app<'a>(apps: &'a [InstalledApp], query: &str) -> Vec<&'a InstalledApp> {
    let (q, stem) = normalize_query(query);
    if q.is_empty() {
        return Vec::new();
    }
    let exact: Vec<&InstalledApp> = apps.iter().filter(|a| is_exact_match(a, query)).collect();
    if exact.iter().any(|a| !a.is_system) {
        return exact;
    }
    let mut out = exact;
    for a in apps {
        let loose = a.name.to_lowercase().contains(&stem)
            || a.bundle_id
                .as_deref()
                .is_some_and(|id| id.to_lowercase().contains(&q))
            || file_stem(&a.path).to_lowercase().contains(&stem);
        if loose && !out.iter().any(|o| std::ptr::eq(*o, a)) {
            out.push(a);
        }
    }
    out
}

/// 查询串的规范化：去空白、转小写、展开开头的 `~`、去掉结尾的 `/`。
/// 返回（整串，去掉 `.app` 后缀的名字部分）。
fn normalize_query(query: &str) -> (String, String) {
    let q = query.trim().to_lowercase();
    let q = match (q.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().to_lowercase(),
        _ => q,
    };
    let q = q.trim_end_matches('/').to_string();
    let stem = q.strip_suffix(".app").unwrap_or(&q).to_string();
    (q, stem)
}

/// 这条查询是不是**完全**指向这个 App：名字、bundle id 或路径相等。
/// 大小写、结尾的 `/` 和 `.app` 后缀都算噪声。`find_app` 用它挑精确命中；
/// CLI 在没人看着的场合（`--yes` / `--json`）也用它决定要不要接受匹配。
pub fn is_exact_match(app: &InstalledApp, query: &str) -> bool {
    let (q, stem) = normalize_query(query);
    if q.is_empty() {
        return false;
    }
    app.name.to_lowercase() == stem
        || app
            .bundle_id
            .as_deref()
            .is_some_and(|id| id.to_lowercase() == q)
        || file_stem(&app.path).to_lowercase() == stem
        || app.path.to_string_lossy().to_lowercase() == q
}

/// 读一个 plist，防三种炸弹：
/// - 大文件：只打开一次（`O_NOFOLLOW | O_NONBLOCK`，不跟链接、碰上 FIFO 不卡住），`fstat` 确认是普通文件、
///   不超过 1 MB，再最多读 1 MB + 1 字节进内存——打开后文件被换掉或变大也读不多；
/// - 引用炸弹：二进制 plist 里的数组可以反复引用同一个对象，130 字节就能展开成 2^30 个值。
///   先用事件流数一遍，超过 `PLIST_EVENT_LIMIT` 就放弃，不去建那棵树；
/// - 深嵌套：几万层的数组建成 `Value` 后，析构是递归的，会把栈撑爆。层数超过 `PLIST_DEPTH_LIMIT` 也放弃。
pub(crate) fn read_plist_bounded(path: &Path) -> Option<plist::Value> {
    use std::os::unix::fs::OpenOptionsExt;
    const LIMIT: u64 = 1024 * 1024;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    plist_is_tame(&bytes).then_some(())?;
    plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()
}

/// 一个 plist 展开后最多这么多个事件（一个值或一个集合算一个）。正常的 Info.plist 几百个。
const PLIST_EVENT_LIMIT: usize = 200_000;
/// 最多嵌套这么多层。
const PLIST_DEPTH_LIMIT: usize = 256;

/// 数一遍事件：不超过上限、不嵌套太深、能读完，才值得建 `Value`。
/// `plist::stream::Reader` 只在 plist 的不稳定特性下公开，这里改走它稳定的 serde 反序列化器：
/// 它同样按需从事件流里取，计数一超限就报错停下，不会先把整棵树展开。
pub(crate) fn plist_is_tame(bytes: &[u8]) -> bool {
    plist::from_bytes::<TamePlist>(bytes).is_ok()
}

/// 只用来数事件的"类型"：反序列化成功说明没超限。
struct TamePlist;

impl<'de> serde::Deserialize<'de> for TamePlist {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let left = std::cell::Cell::new(PLIST_EVENT_LIMIT);
        serde::de::DeserializeSeed::deserialize(
            Tally {
                left: &left,
                depth: 0,
            },
            d,
        )?;
        Ok(TamePlist)
    }
}

/// 走一个值：每个值、每个集合各扣一次额度。
struct Tally<'a> {
    left: &'a std::cell::Cell<usize>,
    depth: usize,
}

impl Tally<'_> {
    fn tick<E: serde::de::Error>(&self) -> Result<(), E> {
        match self.left.get().checked_sub(1) {
            Some(left) => {
                self.left.set(left);
                Ok(())
            }
            None => Err(E::custom("too many plist events")),
        }
    }

    fn child<E: serde::de::Error>(&self) -> Result<Tally<'_>, E> {
        if self.depth >= PLIST_DEPTH_LIMIT {
            return Err(E::custom("plist nested too deep"));
        }
        Ok(Tally {
            left: self.left,
            depth: self.depth + 1,
        })
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for Tally<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Tally<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a plist value")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        self.tick()
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        self.tick()
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        self.tick()
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        self.tick()
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        self.tick()
    }
    fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<(), E> {
        self.tick()
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        self.tick()?;
        while seq.next_element_seed(self.child()?)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        self.tick()?;
        while map.next_key_seed(self.child()?)?.is_some() {
            map.next_value_seed(self.child()?)?;
        }
        Ok(())
    }
}

fn read_app_with(
    env: &Env,
    path: &Path,
    caskroom: &HashMap<String, String>,
) -> Option<InstalledApp> {
    let info = read_plist_bounded(&path.join("Contents/Info.plist"))?;
    let dict = info.as_dictionary()?;
    let get = |key: &str| {
        dict.get(key)
            .and_then(|v| v.as_string())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let stem = file_stem(path);
    let name = get("CFBundleDisplayName")
        .or_else(|| get("CFBundleName"))
        .unwrap_or_else(|| stem.clone());
    let bundle_id = get("CFBundleIdentifier");
    let version = get("CFBundleShortVersionString").or_else(|| get("CFBundleVersion"));
    let executable = get("CFBundleExecutable")
        .map(|e| path.join("Contents/MacOS").join(e))
        .filter(|p| p.exists());
    let has_mas_receipt = path.join("Contents/_MASReceipt/receipt").exists();
    let is_system = env.system_app_dirs.iter().any(|d| path.starts_with(d))
        || (bundle_id
            .as_deref()
            .is_some_and(|id| id.starts_with("com.apple."))
            && !has_mas_receipt);

    let embedded_ids = embedded_bundle_ids(path, bundle_id.as_deref());
    let (team_id, app_groups) = if is_system {
        (None, Vec::new())
    } else {
        let info = signing_info(path);
        if signature_is_trusted(path) {
            info
        } else {
            (None, Vec::new())
        }
    };

    let source = if has_mas_receipt {
        AppSource::AppStore
    } else if let Some(token) = path
        .file_name()
        .and_then(|n| caskroom.get(&n.to_string_lossy().to_string()))
    {
        AppSource::Homebrew(token.clone())
    } else {
        AppSource::Other
    };

    Some(InstalledApp {
        path: path.to_path_buf(),
        name,
        bundle_id,
        version,
        executable,
        team_id,
        embedded_ids,
        app_groups,
        source,
        size: Size::Unknown,
        is_system,
    })
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "app") && path.join("Contents/Info.plist").is_file()
}

fn list_dir(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Homebrew Caskroom 里装着的 `.app` 文件名 → cask token。
fn caskroom_apps(env: &Env) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(prefix) = &env.homebrew_prefix else {
        return out;
    };
    for token_dir in list_dir(&prefix.join("Caskroom")) {
        let token = file_stem(&token_dir);
        for version_dir in list_dir(&token_dir) {
            for entry in list_dir(&version_dir) {
                if entry.extension().is_some_and(|e| e == "app") {
                    if let Some(name) = entry.file_name() {
                        out.insert(name.to_string_lossy().to_string(), token.clone());
                    }
                }
            }
        }
    }
    out
}

/// 包内 Frameworks / PlugIns / XPCServices / Helpers / LoginItems 等的 bundle id，去重，不含 App 自己的。
fn embedded_bundle_ids(app: &Path, own_id: Option<&str>) -> Vec<String> {
    const FOLDERS: [&str; 7] = [
        "Frameworks",
        "PlugIns",
        "XPCServices",
        "Helpers",
        "Library/LoginItems",
        "Library/LaunchServices",
        "MacOS",
    ];
    const BUNDLE_EXTENSIONS: [&str; 7] = [
        "framework",
        "bundle",
        "xpc",
        "app",
        "appex",
        "plugin",
        "systemextension",
    ];
    let mut ids = BTreeSet::new();
    for folder in FOLDERS {
        for entry in list_dir(&app.join("Contents").join(folder)) {
            if !entry.is_dir()
                || !entry
                    .extension()
                    .is_some_and(|e| BUNDLE_EXTENSIONS.contains(&&*e.to_string_lossy()))
            {
                continue;
            }
            if let Some(id) = bundle_identifier_of(&entry) {
                if Some(id.as_str()) != own_id {
                    ids.insert(id);
                }
            }
        }
    }
    ids.into_iter().collect()
}

/// 一个 bundle 的 CFBundleIdentifier，不管它是 `Contents/Info.plist` 还是 framework 那种扁平布局。
fn bundle_identifier_of(bundle: &Path) -> Option<String> {
    let mut candidates = vec![
        bundle.join("Contents/Info.plist"),
        bundle.join("Resources/Info.plist"),
        bundle.join("Versions/Current/Resources/Info.plist"),
        bundle.join("Info.plist"),
    ];
    candidates.extend(
        list_dir(&bundle.join("Versions"))
            .into_iter()
            .map(|v| v.join("Resources/Info.plist")),
    );
    for plist_path in candidates {
        if !plist_path.is_file() {
            continue;
        }
        if let Some(value) = read_plist_bounded(&plist_path) {
            if let Some(id) = value
                .as_dictionary()
                .and_then(|d| d.get("CFBundleIdentifier"))
                .and_then(|v| v.as_string())
            {
                let id = id.trim();
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

const CODESIGN: &str = "/usr/bin/codesign";

/// `codesign` 给的 Team ID 和 App Group；拿不到就是 None / 空。
/// 这只是签名**显示**的内容，谁都能自称；扫描时再对要卸载的 App 验一次签名链（`signature_is_trusted`）。
fn signing_info(app: &Path) -> (Option<String>, Vec<String>) {
    let app_str = app.to_string_lossy().to_string();
    let timeout = Duration::from_secs(10);

    let team_id = run_with_timeout(CODESIGN, &["-dv", &app_str], timeout)
        .and_then(|out| parse_team_id(&String::from_utf8_lossy(&out.stderr)));

    let app_groups = run_with_timeout(
        CODESIGN,
        &["-d", "--entitlements", "-", "--xml", &app_str],
        timeout,
    )
    .and_then(|out| {
        // 输出可能带一小段非 XML 的前缀，从 `<?xml` 或 `bplist` 起读。
        let start = out
            .stdout
            .windows(5)
            .position(|w| w == b"<?xml" || w == b"bplis")
            .unwrap_or(0);
        plist::from_bytes::<plist::Value>(&out.stdout[start..]).ok()
    })
    .and_then(|value| {
        value
            .as_dictionary()?
            .get("com.apple.security.application-groups")?
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_string().map(str::to_string))
                    .collect()
            })
    })
    .unwrap_or_default();

    (team_id, app_groups)
}

/// `codesign -dv` 输出里的 Team ID。Apple 发的 Team ID 是 10 位大写字母数字；别的样子都不算，
/// 免得 `-i $'x\nTeamIdentifier=com'` 这种伪造混进来。
fn parse_team_id(text: &str) -> Option<String> {
    let team = text
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))?
        .trim();
    let well_formed = team.len() == 10
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    well_formed.then(|| team.to_string())
}

/// 这个 App 的签名链到 Apple 吗（Developer ID、App Store 或 Apple 自己）？ad-hoc 签名、过期、被改过的都不算。
/// 大 App 验起来慢，15 秒没答案就当不可信。
pub(crate) fn signature_is_trusted(app: &Path) -> bool {
    let app_str = app.to_string_lossy().to_string();
    // `-R=` 必须连写：分开给，codesign 会把 requirement 当成文件名。
    let args = ["--verify", "-R=anchor apple generic", &app_str];
    run_with_timeout(CODESIGN, &args, Duration::from_secs(15))
        .is_some_and(|out| out.status.success())
}

/// 子进程的 stdout / stderr 各最多收这么多，超过就当失败。
const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// 跑一个程序，超时、输出超过 `OUTPUT_LIMIT`、读不完整都杀掉并返回 None。
/// 子进程放在自己的进程组里，杀的时候连它派生的进程一起杀；读输出的线程到期限一定会结束，
/// 哪怕有孙进程还拿着管道不放。
pub(crate) fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<Output> {
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .ok()?;
    let group = child.id() as libc::pid_t;
    let kill_group = || {
        // SAFETY: 只是发信号；组不存在时返回 ESRCH，没有别的副作用。
        unsafe { libc::killpg(group, libc::SIGKILL) };
    };
    let deadline = Instant::now() + timeout;
    let stop = Arc::new(AtomicBool::new(false));
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill_group();
        let _ = child.wait();
        return None;
    };
    let out_reader = spawn_bounded_reader(stdout, deadline, stop.clone());
    let err_reader = spawn_bounded_reader(stderr, deadline, stop.clone());

    let status = loop {
        if stop.load(Ordering::Relaxed) {
            // 输出超限：没必要再等。
            kill_group();
            let _ = child.wait();
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                stop.store(true, Ordering::Relaxed);
                kill_group();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = out_reader.join().ok().flatten();
    let stderr = err_reader.join().ok().flatten();
    match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => Some(Output {
            status,
            stdout,
            stderr,
        }),
        _ => {
            // 输出没读完整（超限、到期限还有孙进程拿着管道）：把剩下的进程也收掉。
            kill_group();
            None
        }
    }
}

/// 非阻塞地读一根管道，直到读完（Some）、超过 `OUTPUT_LIMIT`、到期限或被叫停（None）。
/// 超限时顺手叫停，让主线程不必等到超时。
fn spawn_bounded_reader<R>(
    pipe: R,
    deadline: Instant,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Option<Vec<u8>>>
where
    R: Read + std::os::unix::io::AsRawFd + Send + 'static,
{
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let fd = pipe.as_raw_fd();
        // SAFETY: fd 属于还活着的 `pipe`；只改它的状态标志。
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return None;
            }
        }
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return None;
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: 指向一个活着的 pollfd，个数为 1。
            let ready = unsafe { libc::poll(&mut poll, 1, 50) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            if ready == 0 {
                continue;
            }
            match pipe.read(&mut chunk) {
                Ok(0) => return Some(buf),
                Ok(n) => {
                    if buf.len() + n > OUTPUT_LIMIT {
                        stop.store(true, Ordering::Relaxed);
                        return None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return None,
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod testing {
    //! 测试用：在临时目录里搭一个最小的 `.app`。
    use super::*;

    pub fn make_app(dir: &Path, name: &str, bundle_id: &str) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleIdentifier".into(), bundle_id.into());
        dict.insert("CFBundleName".into(), name.into());
        dict.insert("CFBundleExecutable".into(), name.into());
        dict.insert("CFBundleShortVersionString".into(), "1.0".into());
        plist::Value::Dictionary(dict)
            .to_file_xml(app.join("Contents/Info.plist"))
            .unwrap();
        std::fs::write(app.join("Contents/MacOS").join(name), b"#!/bin/sh\n").unwrap();
        app
    }

    pub fn make_embedded(app: &Path, folder: &str, bundle_name: &str, bundle_id: &str) -> PathBuf {
        let bundle = app.join("Contents").join(folder).join(bundle_name);
        let plist_path = if bundle_name.ends_with(".framework") {
            bundle.join("Resources/Info.plist")
        } else {
            bundle.join("Contents/Info.plist")
        };
        std::fs::create_dir_all(plist_path.parent().unwrap()).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleIdentifier".into(), bundle_id.into());
        plist::Value::Dictionary(dict)
            .to_file_xml(&plist_path)
            .unwrap();
        bundle
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn scans_apps_with_embedded_ids_and_system_flag() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let apps_dir = &env.app_dirs[0];
        let foo = make_app(apps_dir, "Foo", "com.acme.foo");
        make_embedded(&foo, "PlugIns", "Share.appex", "com.acme.foo.share");
        make_embedded(
            &foo,
            "Frameworks",
            "Sparkle.framework",
            "org.sparkle-project.Sparkle",
        );
        make_embedded(
            &foo,
            "Library/LoginItems",
            "FooAgent.app",
            "com.acme.foo.agent",
        );
        make_app(
            &apps_dir.join("Vendor Folder"),
            "Nested",
            "com.vendor.nested",
        );
        make_app(&env.home.join("Applications"), "Mine", "com.me.mine");
        make_app(&env.system_app_dirs[0], "Mail", "com.apple.mail");
        make_app(apps_dir, "Xcode", "com.apple.dt.Xcode");
        std::fs::create_dir_all(apps_dir.join("Xcode.app/Contents/_MASReceipt")).unwrap();
        std::fs::write(
            apps_dir.join("Xcode.app/Contents/_MASReceipt/receipt"),
            b"r",
        )
        .unwrap();
        make_app(apps_dir, "Safari", "com.apple.Safari");
        std::fs::create_dir_all(apps_dir.join("NotAnApp.app")).unwrap(); // 没有 Info.plist

        let apps = scan_apps(&env);
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Foo", "Mail", "Mine", "Nested", "Safari", "Xcode"],
            "sorted by name, junk skipped"
        );

        let foo = apps.iter().find(|a| a.name == "Foo").unwrap();
        assert_eq!(foo.bundle_id.as_deref(), Some("com.acme.foo"));
        assert_eq!(foo.version.as_deref(), Some("1.0"));
        assert!(foo
            .executable
            .as_ref()
            .unwrap()
            .ends_with("Contents/MacOS/Foo"));
        assert_eq!(
            foo.embedded_ids,
            vec![
                "com.acme.foo.agent",
                "com.acme.foo.share",
                "org.sparkle-project.Sparkle"
            ]
        );
        assert_eq!(foo.source, AppSource::Other);
        assert!(!foo.is_system);
        assert_eq!(foo.size, Size::Unknown);

        assert!(
            apps.iter().find(|a| a.name == "Mail").unwrap().is_system,
            "under /System/Applications"
        );
        assert!(
            apps.iter().find(|a| a.name == "Safari").unwrap().is_system,
            "com.apple without a receipt"
        );
        let xcode = apps.iter().find(|a| a.name == "Xcode").unwrap();
        assert!(
            !xcode.is_system,
            "com.apple from the App Store is removable"
        );
        assert_eq!(xcode.source, AppSource::AppStore);
    }

    #[test]
    fn display_name_wins_and_falls_back_to_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = make_app(&env.app_dirs[0], "Code", "com.microsoft.VSCode");
        let read = read_app(&env, &app).unwrap();
        assert_eq!(read.name, "Code");

        let bare = env.app_dirs[0].join("Bare.app");
        std::fs::create_dir_all(bare.join("Contents")).unwrap();
        plist::Value::Dictionary(plist::Dictionary::new())
            .to_file_xml(bare.join("Contents/Info.plist"))
            .unwrap();
        let read = read_app(&env, &bare).unwrap();
        assert_eq!(read.name, "Bare");
        assert_eq!(read.bundle_id, None);
        assert_eq!(read.executable, None);
    }

    #[test]
    fn homebrew_source_comes_from_the_caskroom() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env::for_test(dir.path());
        let prefix = dir.path().join("opt/homebrew");
        std::fs::create_dir_all(prefix.join("Caskroom/foo-app/1.2.3/Foo.app")).unwrap();
        env.homebrew_prefix = Some(prefix);
        let app = make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        assert_eq!(
            read_app(&env, &app).unwrap().source,
            AppSource::Homebrew("foo-app".into())
        );
    }

    #[test]
    fn finds_apps_exactly_then_loosely() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        make_app(&env.app_dirs[0], "Foo Helper", "com.acme.foohelper");
        make_app(&env.app_dirs[0], "Bar", "com.other.bar");
        make_app(&env.system_app_dirs[0], "Mail", "com.apple.mail");
        make_app(&env.app_dirs[0], "Mailspring", "com.mailspring.Mailspring");
        let apps = scan_apps(&env);

        let names =
            |found: Vec<&InstalledApp>| found.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(find_app(&apps, "foo")),
            vec!["Foo"],
            "an exact name wins over a loose one"
        );
        assert_eq!(names(find_app(&apps, "Foo.app")), vec!["Foo"]);
        assert_eq!(names(find_app(&apps, "COM.ACME.FOO")), vec!["Foo"]);
        assert_eq!(names(find_app(&apps, "fo")), vec!["Foo", "Foo Helper"]);
        assert_eq!(names(find_app(&apps, "acme")), vec!["Foo", "Foo Helper"]);
        assert_eq!(
            names(find_app(
                &apps,
                &env.app_dirs[0].join("Bar.app").display().to_string()
            )),
            vec!["Bar"]
        );
        assert_eq!(
            names(find_app(&apps, "mail")),
            vec!["Mail", "Mailspring"],
            "an exact hit on a system app does not hide the loose ones"
        );
        assert!(find_app(&apps, "").is_empty());
        assert!(find_app(&apps, "zzz").is_empty());

        if let Some(home) = dirs::home_dir() {
            let mut inside_home = Env::for_test(dir.path());
            inside_home.app_dirs = vec![home.join("rind-find-app-test-dir-that-does-not-exist")];
            let mut app = apps[0].clone();
            app.path = home.join("Applications/Tilde.app");
            assert_eq!(
                names(find_app(
                    std::slice::from_ref(&app),
                    "~/Applications/Tilde.app"
                )),
                vec![app.name.clone()],
                "~ expands"
            );
        }
    }

    #[test]
    fn exactness_ignores_noise_but_not_partial_names() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        make_app(&env.app_dirs[0], "Foo Helper", "com.acme.foohelper");
        let apps = scan_apps(&env);
        let foo = apps.iter().find(|a| a.name == "Foo").unwrap();

        for query in ["foo", "Foo.app", "FOO.APP", "com.acme.foo", "COM.Acme.FOO"] {
            assert!(is_exact_match(foo, query), "{query} should be exact");
        }
        let path = foo.path.display().to_string();
        assert!(is_exact_match(foo, &path));
        assert!(
            is_exact_match(foo, &format!("{path}/")),
            "a trailing slash is noise"
        );

        assert!(!is_exact_match(foo, "fo"), "a partial name is not exact");
        assert!(!is_exact_match(foo, "Foo Helper"));
        assert!(!is_exact_match(foo, ""));
    }

    #[test]
    fn team_ids_must_look_like_apple_team_ids() {
        assert_eq!(
            parse_team_id("Identifier=com.acme.foo\nTeamIdentifier=ABCDE12345\n"),
            Some("ABCDE12345".into())
        );
        assert_eq!(parse_team_id("TeamIdentifier=not set"), None);
        assert_eq!(parse_team_id("TeamIdentifier=com"), None, "a made-up team");
        assert_eq!(
            parse_team_id("Identifier=x\nTeamIdentifier=com\nTeamIdentifier=ABCDE12345"),
            None,
            "the first line is what codesign reports"
        );
        assert_eq!(parse_team_id("TeamIdentifier=abcde12345"), None);
        assert_eq!(parse_team_id("nothing here"), None);
    }

    #[test]
    fn an_unsigned_bundle_is_not_trusted_and_a_huge_plist_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        assert!(!signature_is_trusted(&app));
        let read = read_app(&env, &app).unwrap();
        assert_eq!(read.team_id, None, "untrusted signing claims are ignored");
        assert!(
            read.app_groups.is_empty(),
            "untrusted entitlements are ignored"
        );
        assert!(
            signature_is_trusted(Path::new("/System/Applications/Calculator.app"))
                || !Path::new("/System/Applications/Calculator.app").exists()
        );

        let huge = env.app_dirs[0].join("Huge.app");
        std::fs::create_dir_all(huge.join("Contents")).unwrap();
        std::fs::write(
            huge.join("Contents/Info.plist"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(read_app(&env, &huge).is_none());
        assert!(read_plist_bounded(&app.join("Contents/Info.plist")).is_some());
    }

    #[test]
    fn run_with_timeout_kills_a_slow_program() {
        let started = Instant::now();
        assert!(run_with_timeout("/bin/sleep", &["10"], Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        let out = run_with_timeout("/bin/echo", &["hi"], Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert!(run_with_timeout("/definitely/not/here", &[], Duration::from_secs(1)).is_none());
    }

    #[test]
    fn run_with_timeout_caps_output_and_does_not_wait_on_grandchildren() {
        let started = Instant::now();
        assert!(
            run_with_timeout("/usr/bin/yes", &[], Duration::from_secs(30)).is_none(),
            "endless output is cut off"
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "and it does not wait for the timeout: {:?}",
            started.elapsed()
        );

        // 子进程马上退出，但后台的孙进程还拿着 stdout：读线程到期限就停，孙进程也被收掉。
        let started = Instant::now();
        assert!(run_with_timeout(
            "/bin/sh",
            &["-c", "/bin/sleep 30 & echo hi"],
            Duration::from_millis(500)
        )
        .is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// 一个二进制 plist 引用炸弹：第 i 层的数组有两个元素，都引用第 i-1 层，展开后有 2^depth 个叶子。
    fn reference_bomb(depth: usize) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = vec![vec![0x10, 0x00]]; // 整数 0
        for i in 1..=depth {
            let prev = (i - 1) as u8;
            objects.push(vec![0xA2, prev, prev]); // 两个元素的数组
        }
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for object in &objects {
            offsets.push(out.len() as u8);
            out.extend_from_slice(object);
        }
        let table = out.len() as u64;
        out.extend_from_slice(&offsets);
        out.extend_from_slice(&[0; 6]);
        out.push(1); // offset 宽度
        out.push(1); // 引用宽度
        out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
        out.extend_from_slice(&(depth as u64).to_be_bytes()); // 顶层对象
        out.extend_from_slice(&table.to_be_bytes());
        out
    }

    #[test]
    fn a_plist_reference_bomb_is_refused_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.plist");
        std::fs::write(&small, reference_bomb(3)).unwrap();
        assert!(
            read_plist_bounded(&small).is_some(),
            "the same shape, small, is a fine plist"
        );

        // 是计数挡下的：2^16 层展开约 13 万个事件，放行；2^18 约 52 万个，拒绝。
        assert!(plist_is_tame(&reference_bomb(16)));
        assert!(!plist_is_tame(&reference_bomb(18)));

        let bomb = dir.path().join("bomb.plist");
        let bytes = reference_bomb(30);
        assert!(bytes.len() < 200, "{} bytes", bytes.len());
        std::fs::write(&bomb, bytes).unwrap();
        let started = Instant::now();
        assert!(read_plist_bounded(&bomb).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn deep_nesting_links_and_fifos_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("deep.plist");
        let mut xml = String::from("<plist version=\"1.0\">");
        xml.push_str(&"<array>".repeat(10_000));
        xml.push_str(&"</array>".repeat(10_000));
        xml.push_str("</plist>");
        std::fs::write(&deep, xml).unwrap();
        assert!(read_plist_bounded(&deep).is_none());

        let app = make_app(dir.path(), "Foo", "com.acme.foo");
        let link = dir.path().join("link.plist");
        std::os::unix::fs::symlink(app.join("Contents/Info.plist"), &link).unwrap();
        assert!(
            read_plist_bounded(&link).is_none(),
            "links are not followed"
        );

        let fifo = dir.path().join("fifo.plist");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: c 是活着的 C 字符串。
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_plist_bounded(&fifo).is_none(), "a FIFO does not block");
    }
}
