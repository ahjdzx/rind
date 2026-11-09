//! 保护区：无论如何不动的东西。列表见 docs/ARCHITECTURE.md 第 6 步。
//!
//! 判断的是**盘上的真实位置**，不是传进来的字符串：APFS 默认不区分大小写，
//! `~/library/keychains` 和 `~/Library/Keychains` 是同一个地方；中间经过符号链接的路径
//! 指向的也是别处。所以先 `resolve`（相对路径、`..`、读不了的都算解析失败，按保护处理），
//! 再按规则问，而且比较不区分大小写：
//! 1. 具体的保护根（iCloud、钥匙串、Mail……）：路径是它或在它下面。
//! 2. 结构性的根（家目录、`~/Library` 及 macOS 自己的一层子目录、`/Applications` 本身……）：只认路径本身。
//! 3. 系统路径、Rind 自己。
//! 4. 钱包：路径是（或包含、或在）一个装了已知加密钱包扩展的浏览器 profile。

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{Env, ProtectReason};

/// 这条路径（或它的任何祖先）在保护区里吗？解析不了的路径也算在（`Unresolvable`）。
pub fn check(env: &Env, path: &Path) -> Option<ProtectReason> {
    let real = match resolve(env, path) {
        Ok(real) => real,
        Err(why) => return Some(why),
    };
    check_resolved(env, &real)
}

/// 把路径换成它在盘上的真实位置：祖先里的符号链接解析掉、大小写还原成盘上的写法；
/// 叶子本身是符号链接时不跟随（移动的是链接本身）。还不存在的尾巴按字面拼回去。
/// 相对路径、含 `..`、含会被 APFS 折叠成 ASCII 的字符（如开尔文符号 K）、读不了的，都是 `Unresolvable`。
pub fn resolve(env: &Env, path: &Path) -> Result<PathBuf, ProtectReason> {
    let _ = env;
    if !path.is_absolute() {
        return Err(ProtectReason::Unresolvable);
    }
    let mut lexical = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Prefix(_) => lexical.push(part.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(ProtectReason::Unresolvable),
            Component::Normal(name) => {
                if folds_to_ascii(&name.to_string_lossy()) {
                    return Err(ProtectReason::Unresolvable);
                }
                lexical.push(name);
            }
        }
    }

    // 找到存在的最长前缀，剩下的尾巴按字面拼回去。
    let mut existing = lexical.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let meta = loop {
        match std::fs::symlink_metadata(existing) {
            Ok(meta) => break meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(ProtectReason::Unresolvable);
                };
                tail.push(name);
                existing = existing.parent().ok_or(ProtectReason::Unresolvable)?;
            }
            Err(_) => return Err(ProtectReason::Unresolvable),
        }
    };

    let mut real = if meta.file_type().is_symlink() {
        // 链接后面还有尾巴：真实位置在链接指向的地方，这里不替人猜。
        if !tail.is_empty() {
            return Err(ProtectReason::Unresolvable);
        }
        let parent = existing.parent().ok_or(ProtectReason::Unresolvable)?;
        let parent = std::fs::canonicalize(parent).map_err(|_| ProtectReason::Unresolvable)?;
        let name = on_disk_name(&parent, &meta).ok_or(ProtectReason::Unresolvable)?;
        parent.join(name)
    } else {
        std::fs::canonicalize(existing).map_err(|_| ProtectReason::Unresolvable)?
    };
    for name in tail.into_iter().rev() {
        real.push(name);
    }
    Ok(real)
}

/// `path` 已经是 `resolve` 的结果时直接问规则。
fn check_resolved(env: &Env, real: &Path) -> Option<ProtectReason> {
    let canon = canonical_env(env);

    // 1. 具体的保护根。
    for (root, reason) in &canon.roots {
        if is_or_under(real, root) {
            return Some(*reason);
        }
    }
    // 照片 / 音乐库按扩展名认，不管放在哪。
    for part in real.components() {
        if let Component::Normal(name) = part {
            if let Some(reason) = media_library(name) {
                return Some(reason);
            }
        }
    }

    // 2. 结构性的根：只认路径本身。
    if let Some(reason) = structural_root(&canon, real) {
        return Some(reason);
    }

    // 3. 系统路径。
    if is_system_path(&canon, real) {
        return Some(ProtectReason::SystemPath);
    }

    // 4. Rind 自己：数据目录、程序自己、以及用户的废纸篓（History 的东西都在里面）。
    if is_or_under(real, &canon.data_dir)
        || is_inside_own_bundle(real)
        || is_or_under(real, &canon.trash)
    {
        return Some(ProtectReason::RindItself);
    }

    // 5. 钱包。
    if holds_wallet(real) {
        return Some(ProtectReason::Wallet);
    }

    None
}

/// 照片 / 音乐 / TV 库的包名后缀（比较时不区分大小写）。
const PHOTO_LIBRARY_SUFFIXES: &[&str] = &[
    ".photoslibrary",
    ".photolibrary",
    ".migratedphotolibrary",
    ".aplibrary",
];
const MUSIC_LIBRARY_SUFFIXES: &[&str] = &[".musiclibrary", ".tvlibrary"];

/// 这个名字是照片库或音乐库的包吗？
fn media_library(name: &std::ffi::OsStr) -> Option<ProtectReason> {
    let name = name.to_string_lossy().to_lowercase();
    if PHOTO_LIBRARY_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return Some(ProtectReason::PhotoLibrary);
    }
    if MUSIC_LIBRARY_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return Some(ProtectReason::MusicLibrary);
    }
    None
}

/// 子孙探测往下最多看几层。
const DESCENDANT_DEPTH: u8 = 6;
/// 子孙探测最多看多少个条目。
const DESCENDANT_LIMIT: usize = 200_000;

/// 移动一个目录前紧挨着问：它里面（往下 6 层、最多 20 万个条目，不跟符号链接）有没有照片 / 音乐库，
/// 或装了钱包扩展的浏览器 profile？有就按受保护处理——整个目录移走会把它们一起带走。
/// `check` 只看路径本身和它的祖先，扫描时每个候选都要跑，所以不在那里做这么深的查找。
///
/// 看不完（超过上限）时放行，和 `check` 里钱包的 fail closed 不同，理由：
/// - 超限的几乎都是 `node_modules`、`target`、大缓存，一律拦下会让清理基本不能用；
/// - 按层（广度优先）看，浅处的库在额度用完前一定会被看到；照片库是用户手动放的整包，
///   埋在二十万个条目后面的可能性很小；
/// - 钱包仍由 `check` 的规则兜底（4 层内、超限按有钱包处理），那条规则不变。
pub fn check_descendants(path: &Path) -> Option<ProtectReason> {
    probe_descendants(path, DESCENDANT_DEPTH, DESCENDANT_LIMIT)
}

fn probe_descendants(path: &Path, depth: u8, limit: usize) -> Option<ProtectReason> {
    let mut level = vec![path.to_path_buf()];
    let mut seen = 0usize;
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen > limit {
                    return None;
                }
                // `DirEntry::file_type` 不跟符号链接：链接只是链接，移走它不会带走它指向的库。
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                if let Some(reason) = media_library(&entry.file_name()) {
                    return Some(reason);
                }
                let child = entry.path();
                if profile_has_wallet(&child) {
                    return Some(ProtectReason::Wallet);
                }
                next.push(child);
            }
        }
        level = next;
    }
    None
}

/// 全部具体的保护根，给 `rind protected` 和设置页列出来。
pub fn protected_roots(env: &Env) -> Vec<(PathBuf, ProtectReason)> {
    use ProtectReason::*;
    let h = |rest: &str| env.home.join(rest);
    let ul = |rest: &str| env.user_library.join(rest);
    let sl = |rest: &str| env.system_library.join(rest);
    vec![
        (ul("Mobile Documents"), ICloudDrive),
        (ul("CloudStorage"), ICloudDrive),
        (ul("Keychains"), Keychain),
        (sl("Keychains"), Keychain),
        (h(".ssh"), SshKeys),
        (h(".gnupg"), GpgKeys),
        (ul("Mail"), Mail),
        (ul("Containers/com.apple.mail"), Mail),
        (ul("Group Containers/group.com.apple.mail"), Mail),
        (ul("Messages"), Messages),
        (ul("Containers/com.apple.iChat"), Messages),
        (ul("Containers/com.apple.MobileSMS"), Messages),
        (ul("Safari"), Safari),
        (ul("Containers/com.apple.Safari"), Safari),
        (ul("Cookies/Cookies.binarycookies"), Safari),
        (ul("Application Support/AddressBook"), Contacts),
        (ul("Containers/com.apple.AddressBook"), Contacts),
        (ul("Calendars"), Calendars),
        (ul("Containers/com.apple.iCal"), Calendars),
        (ul("Group Containers/group.com.apple.calendar"), Calendars),
        (ul("Containers/com.apple.Notes"), Notes),
        (ul("Group Containers/group.com.apple.notes"), Notes),
        (h("Pictures/Photos Library.photoslibrary"), PhotoLibrary),
        (h("Music/Music"), MusicLibrary),
        (h("Music/iTunes"), MusicLibrary),
        (ul("Application Support/MobileSync"), DeviceBackups),
    ]
}

/// 是 macOS 自己的东西（`com.apple.*` 的文件名，或 Library 下的系统服务目录）吗？
/// 按名字判断，不解析路径：扫描时拿到的名字就是盘上的。
pub fn is_system_owned(env: &Env, path: &Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return false;
    };
    let lower = name.to_lowercase();
    let stem = lower.strip_suffix(".plist").unwrap_or(&lower);
    if stem.starts_with("com.apple.") || stem.starts_with("group.com.apple.") || stem == "com.apple"
    {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    for library in [&env.user_library, &env.system_library] {
        for (sub, names) in SYSTEM_FOLDERS {
            if same_path(parent, &library.join(sub))
                && names.iter().any(|n| n.eq_ignore_ascii_case(&name))
            {
                return true;
            }
        }
    }
    false
}

/// Library 的某个子目录下，直接以普通名字出现的系统服务目录。
const SYSTEM_FOLDERS: &[(&str, &[&str])] = &[
    (
        "Application Support",
        &[
            "AddressBook",
            "App Store",
            "AppleMediaServices",
            "CallHistoryDB",
            "CallHistoryTransactions",
            "CloudDocs",
            "CrashReporter",
            "Dock",
            "FaceTime",
            "FileProvider",
            "iCloud",
            "iLifeMediaBrowser",
            "Knowledge",
            "MobileSync",
            "Siri",
            "SyncServices",
            "TCC",
            "Translation",
            "Apple",
        ],
    ),
    (
        "Caches",
        &[
            "CloudKit",
            "FamilyCircle",
            "GeoServices",
            "Metadata",
            "PassKit",
            "TemporaryItems",
            "Apple",
        ],
    ),
    ("Preferences", &["ByHost", "SystemConfiguration"]),
    (
        "Logs",
        &["DiagnosticReports", "CoreSimulator", "CrashReporter"],
    ),
    (
        "",
        &[
            "Accounts",
            "Assistant",
            "Biome",
            "Daemon Containers",
            "IdentityServices",
            "Sharing",
            "Suggestions",
        ],
    ),
];

/// macOS 自己放在 `~/Library` 一层的文件夹。它们本身永远不动；里面的东西按证据来。
const USER_LIBRARY_FOLDERS: &[&str] = &[
    "Accessibility",
    "Accounts",
    "Application Scripts",
    "Application Support",
    "Assistant",
    "Assistants",
    "Audio",
    "Autosave Information",
    "Biome",
    "Caches",
    "Calendars",
    "CallServices",
    "CloudStorage",
    "ColorPickers",
    "Colors",
    "Compositions",
    "Contacts",
    "ContainerManager",
    "Containers",
    "Cookies",
    "CoreFollowUp",
    "Daemon Containers",
    "DataAccess",
    "DataDeliveryServices",
    "Developer",
    "Dictionaries",
    "DoNotDisturb",
    "DuetExpertCenter",
    "Favorites",
    "Finance",
    "FinanceBackup",
    "FontCollections",
    "Fonts",
    "FrontBoard",
    "GameKit",
    "Group Containers",
    "GroupContainersAlias",
    "HomeKit",
    "HTTPStorages",
    "IdentityServices",
    "Input Methods",
    "IntelligencePlatform",
    "Intents",
    "Internet Plug-Ins",
    "Keyboard",
    "Keyboard Layouts",
    "KeyboardServices",
    "Keychains",
    "LanguageModeling",
    "LaunchAgents",
    "LockdownMode",
    "Logs",
    "Mail",
    "Maps",
    "MediaAnalysis",
    "Messages",
    "Metadata",
    "Mobile Documents",
    "News",
    "Passes",
    "PersonalizationPortrait",
    "Photos",
    "PreferencePanes",
    "Preferences",
    "Printers",
    "PrivateFrameworks",
    "QuickLook",
    "Reminders",
    "Safari",
    "Saved Application State",
    "Screen Savers",
    "ScreenRecordings",
    "Scripts",
    "Services",
    "Sharing",
    "Shortcuts",
    "Sounds",
    "Spelling",
    "StatusKit",
    "Staging",
    "Suggestions",
    "SyncedPreferences",
    "Translation",
    "Trial",
    "UnifiedAssetFramework",
    "Weather",
    "WebKit",
    "Widgets",
    "com.apple.internal.ck",
    "homeenergyd",
    "studentd",
];

/// `Env` 的各个根，解析成盘上的真实位置（`/tmp` 是 `/private/tmp`）。按 Env 缓存，扫描时每个候选都要用。
struct CanonicalEnv {
    system_root: PathBuf,
    home: PathBuf,
    user_library: PathBuf,
    system_library: PathBuf,
    app_dirs: Vec<PathBuf>,
    system_app_dirs: Vec<PathBuf>,
    data_dir: PathBuf,
    receipts_dir: PathBuf,
    trash: PathBuf,
    roots: Vec<(PathBuf, ProtectReason)>,
}

/// 按 (家目录, /Library) 记住每个 Env 解析过的根。
type CanonicalCache = Mutex<HashMap<(PathBuf, PathBuf), Arc<CanonicalEnv>>>;

fn canonical_env(env: &Env) -> Arc<CanonicalEnv> {
    static CACHE: OnceLock<CanonicalCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (env.home.clone(), env.system_library.clone());
    if let Some(found) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return found;
    }
    let canon = |p: &Path| resolve(env, p).unwrap_or_else(|_| p.to_path_buf());
    let built = Arc::new(CanonicalEnv {
        system_root: canon(env.system_library.parent().unwrap_or(Path::new("/"))),
        home: canon(&env.home),
        user_library: canon(&env.user_library),
        system_library: canon(&env.system_library),
        app_dirs: env.app_dirs.iter().map(|p| canon(p)).collect(),
        system_app_dirs: env.system_app_dirs.iter().map(|p| canon(p)).collect(),
        data_dir: canon(&env.data_dir),
        receipts_dir: canon(&env.receipts_dir),
        trash: canon(&env.home.join(".Trash")),
        roots: protected_roots(env)
            .into_iter()
            .map(|(p, r)| (canon(&p), r))
            .collect(),
    });
    if let Ok(mut c) = cache.lock() {
        c.insert(key, built.clone());
    }
    built
}

fn structural_root(canon: &CanonicalEnv, path: &Path) -> Option<ProtectReason> {
    use ProtectReason::*;
    if path == Path::new("/")
        || same_path(path, &canon.home)
        || same_path(path, &canon.user_library)
        || same_path(path, &canon.system_library)
    {
        return Some(RootFolder);
    }
    for dir in canon.app_dirs.iter().chain(canon.system_app_dirs.iter()) {
        if same_path(path, dir) {
            return Some(RootFolder);
        }
    }
    // `/Library` 的一层目录本身都是系统的；`~/Library` 下只有 macOS 自己的那些算，
    // `~/Library/com.tencent.meeting` 这种 App 直接放在 Library 里的东西不算。
    if let Some(parent) = path.parent() {
        if same_path(parent, &canon.system_library) {
            return Some(RootFolder);
        }
        if same_path(parent, &canon.user_library) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if USER_LIBRARY_FOLDERS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&name))
            {
                return Some(RootFolder);
            }
        }
    }
    if same_path(path, &canon.receipts_dir) {
        return Some(RootFolder);
    }
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
        if same_path(path, &canon.home.join(folder)) {
            return Some(UserFolder);
        }
    }
    None
}

fn is_system_path(canon: &CanonicalEnv, path: &Path) -> bool {
    let root = &canon.system_root;
    for sys in [
        "System",
        "usr",
        "bin",
        "sbin",
        "etc",
        "private/etc",
        "Library/Apple",
    ] {
        if is_or_under(path, &root.join(sys)) {
            return true;
        }
    }
    // `/private/var` 整体是系统的，只放开收据目录里的直接子项。
    for var in ["var", "private/var"] {
        let var = root.join(var);
        if is_or_under(path, &var) {
            let in_receipts = path
                .parent()
                .is_some_and(|p| same_path(p, &canon.receipts_dir));
            return !in_receipts;
        }
    }
    false
}

fn is_inside_own_bundle(path: &Path) -> bool {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    let Some(exe) = EXE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(p).ok())
    }) else {
        return false;
    };
    let bundle = exe
        .ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "app"));
    match bundle {
        Some(bundle) => is_or_under(path, bundle),
        None => same_path(path, exe),
    }
}

/// 已知加密钱包扩展：Chromium 系按扩展 id，Firefox 按 xpi 名。
const WALLET_EXTENSION_IDS: &[&str] = &[
    "nkbihfbeogaeaoehlefnkodbefgpgknn", // MetaMask
    "bfnaelmomeimhlpmgjnjophhpkkoljpa", // Phantom
    "hnfanknocfeofbddgcijnmhnfnkdnaad", // Coinbase Wallet
    "egjidjbpglichdcondbcbdnbeeppgdph", // Trust Wallet
    "acmacodkjbdgmoleebolmdjonilkdbch", // Rabby
    "fhbohimaelbohpjbbldcngcnapndodjp", // Binance Wallet
    "aeachknmefphepccionboohckonoeemg", // Coin98
    "ibnejdfjmmkpcnlpebklmnkoeoihofec", // TronLink
];
const FIREFOX_WALLET_XPIS: &[&str] = &["webextension@metamask.io.xpi"];

/// 往下最多看几层：`Application Support/Google/Chrome/Default/Local Extension Settings/<id>` 是 4 层。
const WALLET_SEARCH_DEPTH: u8 = 4;
/// 往下看的目录总数上限。超过它说明这棵树大得不像话，宁可当作有钱包也不放过。
const WALLET_SEARCH_LIMIT: usize = 200_000;

/// 路径本身是钱包数据、在一个带钱包的 profile 里、或者包含这样的 profile。
fn holds_wallet(path: &Path) -> bool {
    // 在 profile 里：往上找 `Local Extension Settings/<id>` 或 `extensions/<xpi>`。
    for ancestor in path.ancestors() {
        if profile_has_wallet(ancestor) {
            return true;
        }
    }
    if !path.is_dir() {
        return false;
    }
    // 包含 profile：先按已知浏览器的布局定点探一遍（快），再做完整的、限深的查找。
    if targeted_wallet_probe(path) {
        return true;
    }
    let mut seen = 0usize;
    match subtree_has_wallet(path, WALLET_SEARCH_DEPTH, &mut seen) {
        Probe::Found | Probe::TooBig => true,
        Probe::Clean => false,
    }
}

/// Chromium 的 profile 可能在 `path` 下 0 到 3 层（`Chrome/Default`、`Google/Chrome/Default`……），
/// Firefox 的在 `.../Profiles/<x>`。
fn targeted_wallet_probe(path: &Path) -> bool {
    let mut level: Vec<PathBuf> = vec![path.to_path_buf()];
    for _ in 0..WALLET_SEARCH_DEPTH {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let child = entry.path();
                if profile_has_wallet(&child) {
                    return true;
                }
                next.push(child);
            }
            if next.len() > 2_000 {
                // 太宽了，定点探测不划算；交给后面完整的查找。
                return false;
            }
        }
        level = next;
    }
    false
}

enum Probe {
    Found,
    Clean,
    TooBig,
}

fn profile_has_wallet(dir: &Path) -> bool {
    let settings = dir.join("Local Extension Settings");
    if settings.is_dir()
        && WALLET_EXTENSION_IDS
            .iter()
            .any(|id| settings.join(id).exists())
    {
        return true;
    }
    let extensions = dir.join("extensions");
    extensions.is_dir()
        && FIREFOX_WALLET_XPIS
            .iter()
            .any(|xpi| extensions.join(xpi).exists())
}

/// 完整地看 `depth` 层里的每个目录（不跟符号链接）。目录数超过上限 → `TooBig`，由调用方按有钱包处理。
fn subtree_has_wallet(dir: &Path, depth: u8, seen: &mut usize) -> Probe {
    if depth == 0 {
        return Probe::Clean;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Probe::Clean;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        *seen += 1;
        if *seen > WALLET_SEARCH_LIMIT {
            return Probe::TooBig;
        }
        let path = entry.path();
        if profile_has_wallet(&path) {
            return Probe::Found;
        }
        match subtree_has_wallet(&path, depth - 1, seen) {
            Probe::Clean => {}
            other => return other,
        }
    }
    Probe::Clean
}

/// 在 `parent` 里按 inode 找回一个条目在盘上的名字（它的大小写可能和传进来的不同）。
fn on_disk_name(parent: &Path, meta: &std::fs::Metadata) -> Option<std::ffi::OsString> {
    std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .find(|e| {
            e.metadata()
                .map(|m| m.ino() == meta.ino() && m.dev() == meta.dev())
                .unwrap_or(false)
        })
        .map(|e| e.file_name())
}

/// 这个名字里有没有会被 APFS 折叠成 ASCII 字母的非 ASCII 字符（开尔文符号 K、长 s ſ、土耳其 ı/İ）。
/// 有的话按名字比较就不可靠，整条路径按解析失败处理。
fn folds_to_ascii(name: &str) -> bool {
    name.chars().any(|c| {
        if c.is_ascii() {
            return false;
        }
        let lower: Vec<char> = c.to_lowercase().collect();
        let upper: Vec<char> = c.to_uppercase().collect();
        matches!(lower.as_slice(), [l] if l.is_ascii_alphabetic())
            || matches!(upper.as_slice(), [u] if u.is_ascii_alphabetic())
            || matches!(c, '\u{0130}')
    })
}

/// 两条（已解析的）路径是不是同一个地方：逐段比较，不区分大小写。
fn same_path(a: &Path, b: &Path) -> bool {
    let mut a = a.components();
    let mut b = b.components();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if !same_component(x, y) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// `path` 是 `root` 本身，或在它下面。
fn is_or_under(path: &Path, root: &Path) -> bool {
    let mut p = path.components();
    for r in root.components() {
        match p.next() {
            Some(c) if same_component(c, r) => {}
            _ => return false,
        }
    }
    true
}

fn same_component(a: Component, b: Component) -> bool {
    match (a, b) {
        (Component::Normal(x), Component::Normal(y)) => {
            x == y || x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase()
        }
        (x, y) => x == y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use ProtectReason::*;

    fn env() -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        fs::create_dir_all(&env.user_library).unwrap();
        (dir, env)
    }

    #[test]
    fn protected_roots_and_their_children_are_refused() {
        let (_d, env) = env();
        assert_eq!(
            check(&env, &env.user_library.join("Mobile Documents")),
            Some(ICloudDrive)
        );
        assert_eq!(
            check(
                &env,
                &env.user_library.join("Mobile Documents/com~apple~Notes/x")
            ),
            Some(ICloudDrive)
        );
        assert_eq!(
            check(&env, &env.home.join(".ssh/id_ed25519")),
            Some(SshKeys)
        );
        assert_eq!(
            check(&env, &env.user_library.join("Keychains/login.keychain-db")),
            Some(Keychain)
        );
        assert_eq!(
            check(
                &env,
                &env.user_library
                    .join("Application Support/MobileSync/Backup")
            ),
            Some(DeviceBackups)
        );
        assert_eq!(
            check(
                &env,
                &env.home.join("Pictures/Photos Library.photoslibrary")
            ),
            Some(PhotoLibrary)
        );
        assert_eq!(
            check(&env, &env.home.join("Pictures/old.photoslibrary/db")),
            Some(PhotoLibrary)
        );
    }

    #[test]
    fn structural_roots_only_match_themselves() {
        let (_d, env) = env();
        assert_eq!(check(&env, &env.home), Some(RootFolder));
        assert_eq!(check(&env, &env.user_library), Some(RootFolder));
        assert_eq!(
            check(&env, &env.user_library.join("Caches")),
            Some(RootFolder)
        );
        assert_eq!(
            check(&env, &env.user_library.join("Caches/com.foo.bar")),
            None
        );
        assert_eq!(
            check(&env, &env.user_library.join("Group Containers")),
            Some(RootFolder)
        );
        assert_eq!(
            check(&env, &env.user_library.join("com.tencent.meeting")),
            None,
            "an app's own folder in ~/Library"
        );
        assert_eq!(check(&env, &env.user_library.join("DBeaverData")), None);
        assert_eq!(
            check(&env, &env.system_library.join("Google")),
            Some(RootFolder),
            "/Library's children are all macOS's"
        );
        assert_eq!(
            check(&env, &env.user_library.join("Cookies")),
            Some(RootFolder)
        );
        assert_eq!(
            check(
                &env,
                &env.user_library.join("Cookies/Cookies.binarycookies")
            ),
            Some(Safari)
        );
        assert_eq!(
            check(
                &env,
                &env.user_library.join("Cookies/com.foo.bar.binarycookies")
            ),
            None
        );
        assert_eq!(check(&env, &env.home.join("Downloads")), Some(UserFolder));
        assert_eq!(check(&env, &env.home.join("Downloads/Foo.dmg")), None);
        assert_eq!(check(&env, &env.app_dirs[0]), Some(RootFolder));
        assert_eq!(check(&env, &env.app_dirs[0].join("Foo.app")), None);
    }

    #[test]
    fn system_paths_except_receipts() {
        let (_d, env) = env();
        let root = env.system_library.parent().unwrap();
        assert_eq!(
            check(&env, &root.join("System/Applications/Mail.app")),
            Some(SystemPath)
        );
        assert_eq!(
            check(&env, &root.join("usr/local/bin/foo")),
            Some(SystemPath)
        );
        assert_eq!(
            check(&env, &root.join("private/var/db/foo")),
            Some(SystemPath)
        );
        assert_eq!(check(&env, &env.receipts_dir), Some(RootFolder));
        assert_eq!(check(&env, &env.receipts_dir.join("com.foo.pkg.bom")), None);
        assert_eq!(
            check(
                &env,
                &env.system_library.join("LaunchDaemons/com.foo.plist")
            ),
            None
        );
    }

    #[test]
    fn rind_data_and_the_trash_are_protected() {
        let (_d, env) = env();
        assert_eq!(
            check(&env, &env.data_dir.join("history.jsonl")),
            Some(RindItself)
        );
        assert_eq!(check(&env, &env.home.join(".Trash")), Some(RindItself));
        assert_eq!(
            check(&env, &env.home.join(".Trash/Foo.app")),
            Some(RindItself)
        );
    }

    #[test]
    fn wallet_profiles_are_refused_from_above_and_below() {
        let (_d, env) = env();
        let chrome = env.user_library.join("Application Support/Google/Chrome");
        let wallet =
            chrome.join("Default/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn");
        fs::create_dir_all(&wallet).unwrap();
        fs::create_dir_all(chrome.join("Default/Cache")).unwrap();
        assert_eq!(
            check(&env, &chrome),
            Some(Wallet),
            "the browser folder holds a wallet profile"
        );
        assert_eq!(
            check(&env, &chrome.join("Default")),
            Some(Wallet),
            "the profile itself"
        );
        assert_eq!(
            check(&env, &chrome.join("Default/Cache")),
            Some(Wallet),
            "inside the profile"
        );
        assert_eq!(
            check(&env, chrome.parent().unwrap()),
            Some(Wallet),
            "the vendor folder above it"
        );

        let firefox = env
            .user_library
            .join("Application Support/Firefox/Profiles/abc.default");
        fs::create_dir_all(firefox.join("extensions")).unwrap();
        fs::write(firefox.join("extensions/webextension@metamask.io.xpi"), b"").unwrap();
        assert_eq!(check(&env, &firefox), Some(Wallet));
        assert_eq!(
            check(&env, &env.user_library.join("Application Support/Firefox")),
            Some(Wallet)
        );

        let clean = env
            .user_library
            .join("Application Support/Google/Chrome Canary/Default");
        fs::create_dir_all(clean.join("Local Extension Settings/other")).unwrap();
        assert_eq!(check(&env, &clean), None);
    }

    #[test]
    fn a_wallet_is_found_behind_thousands_of_sibling_entries() {
        // 以前有个 4000 项的上限，超过就当没钱包；现在定点探测先找到它，找不到也宁可拒绝。
        let (_d, env) = env();
        let google = env.user_library.join("Application Support/Google");
        let cache = google.join("Chrome/Default/Cache");
        fs::create_dir_all(&cache).unwrap();
        for i in 0..4_200 {
            fs::write(cache.join(format!("f_{i:06}")), b"").unwrap();
        }
        for i in 0..300 {
            fs::create_dir_all(google.join(format!("Chrome/Default/Service Worker/{i}"))).unwrap();
        }
        let wallet = google.join(
            "Chrome Canary/Default/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn",
        );
        fs::create_dir_all(&wallet).unwrap();
        assert_eq!(check(&env, &google), Some(Wallet));
        assert_eq!(
            check(&env, &google.join("Chrome")),
            None,
            "the sibling browser has no wallet"
        );
    }

    #[test]
    fn an_absurdly_wide_tree_is_refused_rather_than_waved_through() {
        let (_d, env) = env();
        let huge = env.user_library.join("Caches/com.foo.huge");
        fs::create_dir_all(&huge).unwrap();
        let mut seen = WALLET_SEARCH_LIMIT; // 直接从上限开始，省得真建二十万个目录。
        fs::create_dir_all(huge.join("a/b")).unwrap();
        assert!(matches!(
            subtree_has_wallet(&huge, WALLET_SEARCH_DEPTH, &mut seen),
            Probe::TooBig
        ));
    }

    #[test]
    fn libraries_and_wallets_deep_inside_a_folder_are_found() {
        let (_d, env) = env();
        let cache = env.user_library.join("Caches/com.foo");
        fs::create_dir_all(cache.join("a/b/c/d/Old.photolibrary")).unwrap();
        assert_eq!(check_descendants(&cache), Some(PhotoLibrary));
        assert_eq!(
            check(&env, &cache),
            None,
            "the scan-time check does not walk this deep"
        );

        let music = env.user_library.join("Caches/com.bar");
        fs::create_dir_all(music.join("x/Music Library.musiclibrary")).unwrap();
        assert_eq!(check_descendants(&music), Some(MusicLibrary));

        let wallet = env.user_library.join("Caches/com.baz");
        fs::create_dir_all(
            wallet.join("a/b/c/Profile/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn"),
        )
        .unwrap();
        assert_eq!(check_descendants(&wallet), Some(Wallet));

        let clean = env.user_library.join("Caches/com.clean");
        fs::create_dir_all(clean.join("a/b")).unwrap();
        fs::write(clean.join("a/not-a.photoslibrary"), b"").unwrap();
        assert_eq!(check_descendants(&clean), None, "a file is not a library");

        // 链接指向库：移走链接不会带走库。
        std::os::unix::fs::symlink(
            cache.join("a/b/c/d/Old.photolibrary"),
            clean.join("link.photoslibrary"),
        )
        .unwrap();
        assert_eq!(check_descendants(&clean), None);

        let deep = env.user_library.join("Caches/com.deep");
        fs::create_dir_all(deep.join("1/2/3/4/5/6/Too Deep.photoslibrary")).unwrap();
        assert_eq!(check_descendants(&deep), None, "past the depth limit");
    }

    #[test]
    fn a_huge_tree_without_a_library_passes_the_descendant_probe() {
        // 看不完的大目录（node_modules、target）放行；浅处的库在额度用完前就被看到。
        let (_d, env) = env();
        let modules = env.user_library.join("Caches/com.foo/node_modules");
        for i in 0..50 {
            fs::create_dir_all(modules.join(format!("pkg{i}/lib"))).unwrap();
        }
        assert_eq!(probe_descendants(&modules, DESCENDANT_DEPTH, 20), None);
        fs::create_dir_all(modules.parent().unwrap().join("Photos.photoslibrary")).unwrap();
        assert_eq!(
            probe_descendants(modules.parent().unwrap(), DESCENDANT_DEPTH, 20),
            Some(PhotoLibrary),
            "breadth first: the shallow library is seen before the budget runs out"
        );
    }

    #[test]
    fn system_owned_names() {
        let (_d, env) = env();
        assert!(is_system_owned(
            &env,
            &env.user_library.join("Preferences/com.apple.dock.plist")
        ));
        assert!(is_system_owned(
            &env,
            &env.user_library
                .join("Group Containers/group.com.apple.notes")
        ));
        assert!(is_system_owned(
            &env,
            &env.user_library.join("Application Support/Knowledge")
        ));
        assert!(is_system_owned(
            &env,
            &env.user_library.join("Caches/CloudKit")
        ));
        assert!(is_system_owned(&env, &env.user_library.join("Biome")));
        assert!(!is_system_owned(
            &env,
            &env.user_library.join("Application Support/Obsidian")
        ));
        assert!(
            !is_system_owned(&env, &env.user_library.join("Caches/Knowledge")),
            "only directly under its own folder"
        );
    }

    #[test]
    fn dot_dot_and_relative_paths_are_unresolvable() {
        let (_d, env) = env();
        assert_eq!(
            check(&env, &env.user_library.join("Caches/../Keychains")),
            Some(Unresolvable)
        );
        assert_eq!(
            check(&env, &env.user_library.join("Caches/./com.foo")),
            None,
            "a lone dot changes nothing"
        );
        assert_eq!(
            check(&env, Path::new("Library/Keychains/login.keychain-db")),
            Some(Unresolvable)
        );
        assert_eq!(check(&env, Path::new("Caches/com.foo")), Some(Unresolvable));
        assert!(matches!(
            resolve(&env, Path::new("relative")),
            Err(Unresolvable)
        ));
    }

    #[test]
    fn case_differences_do_not_slip_past() {
        // APFS 不区分大小写：这些写法指向的都是真实的保护区。
        let (_d, env) = env();
        fs::create_dir_all(env.user_library.join("Keychains")).unwrap();
        fs::write(env.user_library.join("Keychains/login.keychain-db"), b"k").unwrap();
        fs::create_dir_all(env.home.join(".ssh")).unwrap();
        fs::write(env.home.join(".ssh/id_ed25519"), b"s").unwrap();
        fs::create_dir_all(&env.data_dir).unwrap();

        let lib = env.home.join("library");
        assert_eq!(
            check(&env, &lib.join("keychains/login.keychain-db")),
            Some(Keychain),
            "existing, wrong case"
        );
        assert_eq!(
            check(&env, &env.user_library.join("KEYCHAINS")),
            Some(Keychain)
        );
        assert_eq!(
            check(&env, &env.home.join(".SSH/id_ed25519")),
            Some(SshKeys)
        );
        assert_eq!(
            check(&env, &lib),
            Some(RootFolder),
            "~/library is ~/Library"
        );
        assert_eq!(
            check(&env, &lib.join("application support/Rind/history.jsonl")),
            Some(RindItself)
        );
        assert_eq!(
            check(&env, &lib.join("keychains/not-yet-there")),
            Some(Keychain),
            "a new file inside"
        );
        assert_eq!(
            check(&env, &lib.join("mail/INBOX.mbox")),
            Some(Mail),
            "a root that does not exist yet"
        );
        assert_eq!(
            check(&env, &lib.join("caches/com.foo")),
            None,
            "an ordinary cache, wrong case, is still fine"
        );
    }

    #[test]
    fn symlinks_in_the_middle_are_seen_through_but_a_symlink_leaf_is_just_a_link() {
        let (_d, env) = env();
        let keychains = env.user_library.join("Keychains");
        fs::create_dir_all(keychains.join("sub")).unwrap();
        fs::write(keychains.join("login.keychain-db"), b"k").unwrap();
        let caches = env.user_library.join("Caches");
        fs::create_dir_all(&caches).unwrap();
        symlink(&keychains, caches.join("com.foo.cache")).unwrap();
        symlink(keychains.join("sub"), caches.join("evil")).unwrap();

        assert_eq!(
            check(&env, &caches.join("com.foo.cache/login.keychain-db")),
            Some(Keychain),
            "through the link"
        );
        assert_eq!(
            check(&env, &caches.join("com.foo.cache/new")),
            Some(Unresolvable),
            "a link with a tail"
        );
        assert_eq!(
            check(&env, &caches.join("evil/../login.keychain-db")),
            Some(Unresolvable)
        );
        assert_eq!(
            check(&env, &caches.join("com.foo.cache")),
            None,
            "the link itself may move; it is only a link"
        );
        assert_eq!(
            resolve(&env, &caches.join("COM.FOO.CACHE"))
                .unwrap()
                .file_name()
                .unwrap(),
            "com.foo.cache",
            "a link's on-disk name is restored too"
        );
    }

    #[test]
    fn names_that_fold_to_ascii_are_unresolvable() {
        let (_d, env) = env();
        assert_eq!(
            check(&env, &env.user_library.join("\u{212A}eychains")),
            Some(Unresolvable),
            "Kelvin sign K"
        );
        assert_eq!(
            check(&env, &env.home.join(".\u{17F}sh")),
            Some(Unresolvable),
            "long s"
        );
        assert_eq!(
            check(&env, &env.user_library.join("Caches/微信")),
            None,
            "ordinary non-ASCII is fine"
        );
        assert!(folds_to_ascii("\u{131}"));
        assert!(!folds_to_ascii("Straße"));
    }

    #[test]
    fn resolve_restores_on_disk_case_and_keeps_a_missing_tail() {
        let (_d, env) = env();
        let caches = env.user_library.join("Caches");
        fs::create_dir_all(&caches).unwrap();
        let real = resolve(&env, &env.home.join("library/CACHES/com.foo/new")).unwrap();
        let expected = fs::canonicalize(&caches).unwrap().join("com.foo/new");
        assert_eq!(real, expected);
        assert!(resolve(&env, &env.user_library.join("Caches/x/../y")).is_err());
    }
}
