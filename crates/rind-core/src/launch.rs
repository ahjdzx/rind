//! 启动项：LaunchAgents / LaunchDaemons。思路见 docs/ARCHITECTURE.md「启动项」一节。
//!
//! 契约（GUI / CLI 依赖它，改动要同步）：
//! - `list` 只读：解析 plist（>1 MB 不读），找出所属 App（`LaunchdProgram` 规则 / bundle id），
//!   程序不存在就是孤儿；运行状态来自 `/bin/launchctl`（测试里注入假的）。
//! - 只有当前用户域（`~/Library/LaunchAgents`）的项能停用/启用/移除；`/Library` 下的是
//!   `HoldBack::NeedsAdmin`，`com.apple.*` 是 `SystemOwned`。不收符号链接的 plist；`~/Library/LaunchAgents`
//!   本身或它到家目录之间的哪一级是符号链接时，整个用户域都不列（否则指向 `/Library/LaunchAgents` 的链接
//!   会让全局项冒充成可管理的）。
//! - label 按原样读（不 trim），只有 `[A-Za-z0-9._-]` 组成的才可管理。
//! - 动手前（停用/启用、移除的 bootout 之前）用 `O_NOFOLLOW` 重新打开 plist 核对：普通文件、≤ 1 MB、属于当前用户、
//!   组和其他人写不了、不是深嵌套的炸弹（和 Info.plist 共用 `apps::plist_is_tame`），盘上的 `Label` 与扫描时完全相同、
//!   程序也没换。任何一点不符就报错，不调用 launchctl。
//! - 核对时记下这份文件的身份（设备 + inode + 大小 + mtime / ctime + 内容哈希）：调用 launchctl 之前、`bootout`
//!   之后、以及真正 `rename` 之前（`remove::trash_items_checked` 的回调，在写 History 之前）各比一次，
//!   变了就不移动（`CHANGED_AT_MOVE`），卸下来的装回去。原地改写再把 mtime 设回去也看得出来（比内容）。
//!   剩下的窗口只有 launchctl 自己读文件的那一瞬间，那是它的接口决定的。
//! - 停用/启用用 `launchctl disable|enable gui/<uid>/<label>`（可逆、不动文件），
//!   并 `bootout` / `bootstrap` 让它立刻生效。
//! - 移除 = 先 `bootout`，再把 plist 走 `remove::trash_items`（记录类型 `RecordKind::LaunchItems`）。
//!   卸下来了却没移走的（被拒、失败、History 出错）重新 `bootstrap` 回去；装不回去就在原因里注明
//!   `UNLOADED_NOT_MOVED`。
//! - 程序在不在：读不了（EACCES/EPERM）的按在，`/Volumes/...` 下的按在（外接盘可能没挂），与孤儿一致。
//!
//! launchctl 抽象成 `Launchctl`：`list` / `set_enabled` / `remove` 用真的 `/bin/launchctl`，
//! `*_with` 版本接受注入的实现，测试里绝不碰真实的 launchd。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::history::Ledger;
use crate::remove::{self, Batch, Removable};
use crate::uninstall::ExecuteError;
use crate::{protect, AppRef, Env, HoldBack, InstalledApp, RecordKind, RemovalOutcome, Source};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDomain {
    /// `~/Library/LaunchAgents`
    UserAgent,
    /// `/Library/LaunchAgents`
    GlobalAgent,
    /// `/Library/LaunchDaemons`
    GlobalDaemon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum LaunchStatus {
    /// 已加载；`pid` 有值表示正在运行。
    Loaded {
        pid: Option<u32>,
    },
    NotLoaded,
    /// 被 `launchctl disable` 停用了。
    Disabled,
    /// 查不到（权限、超时）。
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchItem {
    pub plist: PathBuf,
    pub domain: LaunchDomain,
    pub label: Option<String>,
    /// `Program` 或 `ProgramArguments[0]`。
    pub program: Option<PathBuf>,
    pub program_exists: bool,
    pub run_at_load: bool,
    pub keep_alive: bool,
    /// 所属的已安装 App。
    pub owner: Option<AppRef>,
    /// 没有主人、程序也不在了。
    pub orphan: bool,
    pub status: LaunchStatus,
    pub hold_back: Option<HoldBack>,
}

impl LaunchItem {
    /// 能不能停用/启用/移除。label 里有空格之类的字符也不行。
    pub fn manageable(&self) -> bool {
        self.domain == LaunchDomain::UserAgent
            && self.hold_back.is_none()
            && self.label.as_deref().is_some_and(valid_label)
    }
}

/// 移除时卸下来了、却没能移走也没能重新加载的项，原因里带这句。
pub const UNLOADED_NOT_MOVED: &str = "unloaded but not moved";

/// 核对过之后、动手之前盘上又变了：这一份不动，卸下来的装回去。
pub const CHANGED_AT_MOVE: &str = "the launch item changed again before it could be moved";

/// plist 最多读这么大。
const PLIST_LIMIT: u64 = 1024 * 1024;

/// 核对过的那份 plist 在盘上的身份：设备 + inode + 大小 + mtime / ctime（含纳秒）加上内容的哈希。
/// 只看元数据挡不住"同一个 inode 原地改写、再把 mtime 设回去"（属主做得到），所以内容也要比。
/// 调用 launchctl 之前、bootout 之后、以及真正移动之前各再看一眼，变了就说明盘上换过东西。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    /// 内容的 SipHash（`DefaultHasher`，固定种子）。不是密码学哈希，但改内容又要碰上它就得费点劲。
    hash: u64,
}

impl Fingerprint {
    fn of(meta: &std::fs::Metadata, hash: u64) -> Fingerprint {
        use std::os::unix::fs::MetadataExt;
        Fingerprint {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            hash,
        }
    }

    /// 现在盘上的这一份还是刚才核对过的那一份吗？
    /// 不跟符号链接（被换成链接就不算）；同一个 inode 原地改写、把 mtime 设回去也看得出来（比内容）。
    fn still(&self, path: &Path) -> bool {
        read_checked(path)
            .map(|(meta, bytes)| Fingerprint::of(&meta, hash_bytes(&bytes)) == *self)
            .unwrap_or(false)
    }
}

/// 内容的哈希。不是密码学哈希，但足够发现"同一个 inode 被原地改写"。
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 读一个 plist 之前出岔子的原因。
enum ReadError {
    Open(String),
    NotRegular,
    TooLarge,
    Read(String),
}

impl ReadError {
    fn detail(&self) -> &str {
        match self {
            ReadError::Open(e) | ReadError::Read(e) => e,
            ReadError::NotRegular => "not a regular file",
            ReadError::TooLarge => "too large",
        }
    }
}

/// `O_NOFOLLOW` 打开、确认是普通文件、读出来（最多 `PLIST_LIMIT` 字节）。
/// 核对和之后的每一次复核共用同一段读取，免得两边对"读到的是什么"有不同理解。
fn read_checked(path: &Path) -> Result<(std::fs::Metadata, Vec<u8>), ReadError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK：被换成 FIFO 时不至于卡在 open 上；随后按普通文件检查。
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| ReadError::Open(e.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|e| ReadError::Open(e.to_string()))?;
    if !meta.is_file() {
        return Err(ReadError::NotRegular);
    }
    if meta.len() > PLIST_LIMIT {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(PLIST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ReadError::Read(e.to_string()))?;
    if bytes.len() as u64 > PLIST_LIMIT {
        return Err(ReadError::TooLarge);
    }
    Ok((meta, bytes))
}

/// label 只认这些字符：它会拼进 `gui/<uid>/<label>`，带 `/`、空格的会变成别的目标或对不上 launchd 里的名字。
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 和 launchd 打交道的全部入口。真实实现是 `SystemLaunchctl`；测试注入假的并记下调用。
pub trait Launchctl: Send + Sync {
    /// `launchctl list`：当前用户 gui 域里已加载的任务，label → PID（加载了但没在跑是 None）。
    /// 读不到（超时、出错）返回 None，调用方据此显示"未知"而不是"未加载"。
    fn user_list(&self) -> Option<HashMap<String, Option<u32>>>;
    /// `launchctl print-disabled <target>`（如 `gui/501`、`system`）：label → 是否被停用。读不到返回 None。
    fn disabled(&self, target: &str) -> Option<HashMap<String, bool>>;
    /// 跑一条会改状态的命令（`disable` / `enable` / `bootout` / `bootstrap`），失败带上 launchctl 的输出。
    fn run(&self, args: &[&str]) -> anyhow::Result<()>;
}

/// 真的 `/bin/launchctl`：绝对路径（不信 PATH），每条命令 5 秒超时（launchd 卡住时界面不能跟着卡）。
pub struct SystemLaunchctl;

const LAUNCHCTL: &str = "/bin/launchctl";
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(5);

impl SystemLaunchctl {
    fn query(args: &[&str]) -> Option<String> {
        let out = crate::apps::run_with_timeout(LAUNCHCTL, args, LAUNCHCTL_TIMEOUT)?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Launchctl for SystemLaunchctl {
    fn user_list(&self) -> Option<HashMap<String, Option<u32>>> {
        Some(parse_list(&Self::query(&["list"])?))
    }

    fn disabled(&self, target: &str) -> Option<HashMap<String, bool>> {
        Some(parse_disabled(&Self::query(&["print-disabled", target])?))
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<()> {
        let shown = args.join(" ");
        let out = crate::apps::run_with_timeout(LAUNCHCTL, args, LAUNCHCTL_TIMEOUT)
            .with_context(|| format!("launchctl {shown}: timed out or could not start"))?;
        if !out.status.success() {
            let mut why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if why.is_empty() {
                why = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            anyhow::bail!("launchctl {shown}: {why} ({})", out.status);
        }
        Ok(())
    }
}

/// `launchctl list` 的输出：表头 `PID\tStatus\tLabel`，PID 为 `-` 表示加载了但没在跑。
fn parse_list(text: &str) -> HashMap<String, Option<u32>> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(3, '\t');
            let pid = cols.next()?.trim();
            let _status = cols.next()?;
            let label = cols.next()?.trim();
            if pid == "PID" || label.is_empty() {
                return None;
            }
            Some((label.to_string(), pid.parse().ok()))
        })
        .collect()
}

/// `launchctl print-disabled` 的输出：`"label" => disabled|enabled`；老系统写成 `=> true|false`（true 是停用）。
fn parse_disabled(text: &str) -> HashMap<String, bool> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once("=>")?;
            let label = key.trim().trim_matches('"');
            let disabled = match value.trim() {
                "disabled" | "true" => true,
                "enabled" | "false" => false,
                _ => return None,
            };
            (!label.is_empty()).then(|| (label.to_string(), disabled))
        })
        .collect()
}

fn uid() -> u32 {
    // SAFETY: getuid 没有参数，总是成功。
    unsafe { libc::getuid() }
}

fn gui_domain() -> String {
    format!("gui/{}", uid())
}

/// 列出全部启动项：先孤儿，再按域、名字排。
pub fn list(env: &Env, all_apps: &[InstalledApp]) -> Vec<LaunchItem> {
    list_with(env, all_apps, &SystemLaunchctl)
}

/// 同 `list`，launchctl 由调用方给。状态一次批量取（`list` + `print-disabled`），不逐项问。
pub fn list_with(env: &Env, all_apps: &[InstalledApp], ctl: &dyn Launchctl) -> Vec<LaunchItem> {
    let places = [
        (LaunchDomain::UserAgent, user_agents_dir(env)),
        (
            LaunchDomain::GlobalAgent,
            Some(env.system_library.join("LaunchAgents")),
        ),
        (
            LaunchDomain::GlobalDaemon,
            Some(env.system_library.join("LaunchDaemons")),
        ),
    ];
    // App 包的真实位置算一次：程序路径常经过符号链接（如 /usr/local/bin 指进包里）。
    let bundles: Vec<(PathBuf, &InstalledApp)> = all_apps
        .iter()
        .map(|a| {
            (
                std::fs::canonicalize(&a.path).unwrap_or_else(|_| a.path.clone()),
                a,
            )
        })
        .collect();

    let mut items: Vec<LaunchItem> = places
        .iter()
        .flat_map(|(domain, dir)| {
            dir.as_deref()
                .map(plist_files)
                .unwrap_or_default()
                .into_iter()
                .map(|plist| read_item(env, all_apps, &bundles, *domain, plist))
        })
        .collect();
    if items.is_empty() {
        return items;
    }

    let loaded = ctl.user_list();
    let gui_disabled = ctl.disabled(&gui_domain());
    let system_disabled = items
        .iter()
        .any(|i| i.domain == LaunchDomain::GlobalDaemon)
        .then(|| ctl.disabled("system"))
        .flatten();
    for item in &mut items {
        item.status = status_of(
            item,
            loaded.as_ref(),
            gui_disabled.as_ref(),
            system_disabled.as_ref(),
        );
    }

    items.sort_by(|a, b| {
        let key = |i: &LaunchItem| {
            (
                !i.orphan,
                i.domain,
                i.label.is_none(),
                i.label.as_deref().map(str::to_lowercase),
                i.plist.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    items
}

/// `~/Library/LaunchAgents`；它自己或往上到家目录的哪一级是符号链接就是 None（整个用户域不列、不动）。
fn user_agents_dir(env: &Env) -> Option<PathBuf> {
    let dir = env.user_library.join("LaunchAgents");
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let chain = dir
        .ancestors()
        .take_while(|a| a.starts_with(&env.home))
        .chain([dir.as_path(), env.user_library.as_path()]);
    for p in chain {
        if is_link(p) {
            return None;
        }
    }
    Some(dir)
}

/// 目录里的 `*.plist` 普通文件（不含子目录、不收符号链接）。目录不存在或读不了就是空的。
fn plist_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "plist"))
        .collect()
}

fn read_item(
    env: &Env,
    all_apps: &[InstalledApp],
    bundles: &[(PathBuf, &InstalledApp)],
    domain: LaunchDomain,
    plist: PathBuf,
) -> LaunchItem {
    // 读不了、损坏、超过 1 MB 的都照样列出来：用户该知道它在那儿，只是什么也说不清。
    let value = crate::apps::read_plist_bounded(&plist);
    let dict = value.as_ref().and_then(plist::Value::as_dictionary);
    // 不 trim：带空格的 label 原样留着，`manageable` 会拒绝它，动手前的核对也要求逐字相同。
    let label = dict
        .and_then(|d| d.get("Label"))
        .and_then(plist::Value::as_string)
        .filter(|l| !l.is_empty())
        .map(String::from);
    let program = dict.and_then(crate::matcher::launchd_program_in);
    let run_at_load = dict
        .and_then(|d| d.get("RunAtLoad"))
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false);
    // KeepAlive 是字典时表示"满足条件就保活"，对用户来说一样是常驻。
    let keep_alive = match dict.and_then(|d| d.get("KeepAlive")) {
        Some(plist::Value::Boolean(b)) => *b,
        Some(plist::Value::Dictionary(_)) => true,
        _ => false,
    };
    let program_exists = program.as_deref().is_some_and(program_on_disk);
    let owner = owner_of(program.as_deref(), label.as_deref(), all_apps, bundles);
    // 不知道程序是什么（plist 读不了）就不敢说它是孤儿。
    let orphan = owner.is_none() && program.is_some() && !program_exists;
    let hold_back = hold_back_of(env, domain, &plist, label.as_deref());
    LaunchItem {
        plist,
        domain,
        label,
        program,
        program_exists,
        run_at_load,
        keep_alive,
        owner,
        orphan,
        status: LaunchStatus::Unknown,
        hold_back,
    }
}

/// launchd 的默认 PATH：`ProgramArguments = ["sh", ...]` 这种裸命令名按它找。
const LAUNCHD_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// 按 `symlink_metadata`：链接本身在就算在，不跟随（悬空链接由 launchd 自己报错）。
/// 读不了（EACCES/EPERM）不能断定不在，按在；`/Volumes/...` 下的按在（外接盘可能只是没挂）。与孤儿一致。
fn program_on_disk(program: &Path) -> bool {
    let present = |p: &Path| match std::fs::symlink_metadata(p) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    };
    if program.is_absolute() {
        return program.starts_with("/Volumes") || present(program);
    }
    LAUNCHD_PATH
        .iter()
        .any(|dir| present(&Path::new(dir).join(program)))
}

/// 先看程序在不在某个 App 包里（最可靠），再看 label 是不是某个 App 的 bundle id / 内嵌 id 或以它为前缀。
fn owner_of(
    program: Option<&Path>,
    label: Option<&str>,
    all_apps: &[InstalledApp],
    bundles: &[(PathBuf, &InstalledApp)],
) -> Option<AppRef> {
    if let Some(program) = program.filter(|p| p.is_absolute()) {
        let real = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
        // 包套包时取最里面的那个。
        let inside = bundles
            .iter()
            .filter(|(bundle, app)| real.starts_with(bundle) || program.starts_with(&app.path))
            .max_by_key(|(bundle, _)| bundle.as_os_str().len());
        if let Some((_, app)) = inside {
            return Some(app.as_ref());
        }
    }
    let label = label?.to_lowercase();
    all_apps
        .iter()
        .flat_map(|app| {
            app.bundle_id
                .iter()
                .chain(app.embedded_ids.iter())
                .map(move |id| (id.to_lowercase(), app))
        })
        // 只认整段：`com.acme.foo` 不能认领 `com.acme.foobar`。
        .filter(|(id, _)| {
            !id.is_empty()
                && (label == *id
                    || label
                        .strip_prefix(id.as_str())
                        .is_some_and(|rest| rest.starts_with('.')))
        })
        .max_by_key(|(id, _)| id.len())
        .map(|(_, app)| app.as_ref())
}

fn owned_by_me(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.uid() == uid())
}

fn hold_back_of(
    env: &Env,
    domain: LaunchDomain,
    plist: &Path,
    label: Option<&str>,
) -> Option<HoldBack> {
    let apple_label = label.is_some_and(|l| l.to_lowercase().starts_with("com.apple."));
    if apple_label || protect::is_system_owned(env, plist) {
        return Some(HoldBack::SystemOwned);
    }
    // `/Library` 下的属于 root 或所有用户：MVP 不提权。用户域里别人（多是 root）的 plist 也一样。
    if domain != LaunchDomain::UserAgent || !owned_by_me(plist) {
        return Some(HoldBack::NeedsAdmin);
    }
    protect::check(env, plist).map(|why| HoldBack::Protected { why })
}

/// 停用表优先：它是持久的开关，界面上的"启用/停用"反映的就是它。
fn status_of(
    item: &LaunchItem,
    loaded: Option<&HashMap<String, Option<u32>>>,
    gui_disabled: Option<&HashMap<String, bool>>,
    system_disabled: Option<&HashMap<String, bool>>,
) -> LaunchStatus {
    let Some(label) = item.label.as_deref() else {
        return LaunchStatus::Unknown;
    };
    let is_disabled = |table: Option<&HashMap<String, bool>>| {
        table.and_then(|t| t.get(label)).copied() == Some(true)
    };
    match item.domain {
        LaunchDomain::UserAgent | LaunchDomain::GlobalAgent => {
            if is_disabled(gui_disabled) {
                return LaunchStatus::Disabled;
            }
            match loaded {
                Some(loaded) => match loaded.get(label) {
                    Some(pid) => LaunchStatus::Loaded { pid: *pid },
                    None => LaunchStatus::NotLoaded,
                },
                None => LaunchStatus::Unknown,
            }
        }
        // 系统域的加载情况普通用户看不到，只有停用表能读。
        LaunchDomain::GlobalDaemon if is_disabled(system_disabled) => LaunchStatus::Disabled,
        LaunchDomain::GlobalDaemon => LaunchStatus::Unknown,
    }
}

/// 动手前再核一次：传进来的 `LaunchItem` 可能是旧的，也可能是拼出来的。
/// 返回 (label, plist 路径字符串, 盘上那份的身份)；不行就说为什么。
fn guard<'a>(env: &Env, item: &'a LaunchItem) -> Result<(&'a str, &'a str, Fingerprint), String> {
    let not_manageable = || "not a manageable launch item".to_string();
    if !item.manageable() {
        return Err(item
            .hold_back
            .as_ref()
            .and_then(|h| remove::refusal(env, Some(h), &item.plist))
            .unwrap_or_else(not_manageable));
    }
    let label = item.label.as_deref().ok_or_else(not_manageable)?;
    let plist = item.plist.to_str().ok_or_else(not_manageable)?;
    // 用户域目录现在是不是符号链接也要再看一次：扫描之后可能被换掉。
    let in_user_agents = user_agents_dir(env).is_some_and(|dir| item.plist.parent() == Some(&dir));
    if !in_user_agents
        || !valid_label(label)
        || label.to_lowercase().starts_with("com.apple.")
        || protect::is_system_owned(env, &item.plist)
    {
        return Err(not_manageable());
    }
    if let Some(why) = protect::check(env, &item.plist) {
        return Err(remove::protected_reason(why));
    }
    let print = verify_on_disk(item, label)?;
    Ok((label, plist, print))
}

/// 用 `O_NOFOLLOW` 重新打开 plist 核对：还是那个普通文件、属于当前用户、组和其他人写不了、不超过 1 MB，
/// 不是深嵌套的炸弹，盘上的 `Label` 与 `label` 逐字相同（不 trim），程序也和扫描时一样。
/// 返回这份文件当时的身份，调用方拿它比对「还是不是同一份」。
fn verify_on_disk(item: &LaunchItem, label: &str) -> Result<Fingerprint, String> {
    use std::os::unix::fs::MetadataExt;

    let shown = item.plist.display();
    let changed = |what: &str| format!("{shown} changed since the scan ({what}); scan again");
    let (meta, bytes) = read_checked(&item.plist).map_err(|e| match e {
        ReadError::NotRegular => changed("not a regular file"),
        ReadError::TooLarge => changed("too large"),
        other => format!("could not read {shown}: {}", other.detail()),
    })?;
    let print = Fingerprint::of(&meta, hash_bytes(&bytes));
    if meta.uid() != uid() {
        return Err(format!("{shown} is not owned by you"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!("{shown} can be written by other users"));
    }
    // 深嵌套的小 plist 能把 `plist::Value::from_reader` 撑爆栈（SIGABRT）。和 Info.plist 一样先数一遍事件。
    if !crate::apps::plist_is_tame(&bytes) {
        return Err(changed("unreadable"));
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|_| changed("unreadable"))?;
    let dict = value.as_dictionary().ok_or_else(|| changed("unreadable"))?;
    if dict.get("Label").and_then(plist::Value::as_string) != Some(label) {
        return Err(changed("label"));
    }
    if crate::matcher::launchd_program_in(dict) != item.program {
        return Err(changed("program"));
    }
    Ok(print)
}

/// 停用或启用一个用户域的启动项。不可管理的项返回错误，不调用 launchctl。
pub fn set_enabled(env: &Env, item: &LaunchItem, enabled: bool) -> anyhow::Result<()> {
    set_enabled_with(env, item, enabled, &SystemLaunchctl)
}

/// 同 `set_enabled`，launchctl 由调用方给。
/// 停用：`disable` 再 `bootout`（没加载时 bootout 会失败，忽略）；
/// 启用：`enable`，还没加载就 `bootstrap`（已加载时 bootstrap 必然报错，所以先看一眼）。
pub fn set_enabled_with(
    env: &Env,
    item: &LaunchItem,
    enabled: bool,
    ctl: &dyn Launchctl,
) -> anyhow::Result<()> {
    let (label, plist, _) = guard(env, item).map_err(anyhow::Error::msg)?;
    let domain = gui_domain();
    let service = format!("{domain}/{label}");
    if enabled {
        ctl.run(&["enable", &service])?;
        let already = ctl.user_list().is_some_and(|l| l.contains_key(label));
        if !already {
            ctl.run(&["bootstrap", &domain, plist])
                .context("enabled, but loading it now failed; it will load at next login")?;
        }
    } else {
        ctl.run(&["disable", &service])?;
        let _ = ctl.run(&["bootout", &domain, plist]);
    }
    Ok(())
}

/// 移除选中的启动项：先 bootout，再把 plist 移进废纸篓。规则同 `remove::trash_items`。
pub fn remove(
    env: &Env,
    items: &[LaunchItem],
    selected: &std::collections::HashSet<PathBuf>,
    source: Source,
    dry_run: bool,
    ledger: Option<&Ledger>,
) -> Result<RemovalOutcome, ExecuteError> {
    remove_with(
        env,
        items,
        selected,
        source,
        dry_run,
        ledger,
        &SystemLaunchctl,
    )
}

/// 同 `remove`，launchctl 由调用方给。不可管理的选中项进 `refused`，不 bootout 也不移动；
/// 选择、History 的前置检查在 bootout 之前做完，免得卸了任务却一个文件都没动。
pub fn remove_with(
    env: &Env,
    items: &[LaunchItem],
    selected: &HashSet<PathBuf>,
    source: Source,
    dry_run: bool,
    ledger: Option<&Ledger>,
    ctl: &dyn Launchctl,
) -> Result<RemovalOutcome, ExecuteError> {
    if selected.is_empty() {
        return Err(ExecuteError::NothingSelected);
    }
    if let Some(stranger) = selected
        .iter()
        .find(|p| !items.iter().any(|i| &i.plist == *p))
    {
        return Err(ExecuteError::NotInPlan(stranger.clone()));
    }
    if !dry_run && ledger.is_none() {
        return Err(ExecuteError::LedgerRequired);
    }

    let mut refused = Vec::new();
    let mut chosen: Vec<(&LaunchItem, &str, Fingerprint)> = Vec::new();
    for item in items.iter().filter(|i| selected.contains(&i.plist)) {
        match guard(env, item) {
            Ok((_, plist, print)) => chosen.push((item, plist, print)),
            Err(why) => refused.push((item.plist.clone(), why)),
        }
    }
    if chosen.is_empty() {
        return Ok(RemovalOutcome {
            refused,
            dry_run,
            ..Default::default()
        });
    }

    let domain = gui_domain();
    // 真卸下来的：移不走时要装回去。本来就没加载的（bootout 失败）不必，也不该被顺手加载。
    let mut unloaded: Vec<(&LaunchItem, &str)> = Vec::new();
    if !dry_run {
        for (item, plist, print) in &chosen {
            let (item, plist) = (*item, *plist);
            // 核对过之后、调用 launchctl 之前再看一眼：盘上换了就不碰它。
            if !print.still(&item.plist) {
                refused.push((item.plist.clone(), CHANGED_AT_MOVE.to_string()));
                continue;
            }
            if ctl.run(&["bootout", &domain, plist]).is_ok() {
                unloaded.push((item, plist));
            }
        }
        // `bootout` 期间盘上又变了：这一份也不移动（`reload_unmoved` 会把它装回去）。
        chosen.retain(|(item, _, print)| {
            if print.still(&item.plist) {
                true
            } else {
                refused.push((item.plist.clone(), CHANGED_AT_MOVE.to_string()));
                false
            }
        });
        // 全都在核对之后变了：一个也不动，卸下来的装回去。
        if chosen.is_empty() {
            let mut outcome = RemovalOutcome {
                refused,
                dry_run,
                ..Default::default()
            };
            reload_unmoved(ctl, &domain, &unloaded, &mut outcome);
            return Ok(outcome);
        }
    }

    let (title, bundle_id) = match chosen.as_slice() {
        [(one, _, _)] => (
            one.label.clone().unwrap_or_default(),
            one.owner.as_ref().and_then(|o| o.bundle_id.clone()),
        ),
        many => (format!("{} launch items", many.len()), None),
    };
    let batch = Batch {
        kind: RecordKind::LaunchItems,
        title,
        bundle_id,
        source,
    };
    let picked: HashSet<PathBuf> = chosen.iter().map(|(i, _, _)| i.plist.clone()).collect();
    // 真正 `rename` 之前再核一次：后一次读取和移动之间只隔着写 History，不是微秒级。
    let expected: Vec<(&LaunchItem, Fingerprint)> = chosen
        .iter()
        .map(|(item, _, print)| (*item, *print))
        .collect();
    let recheck = |path: &Path| -> Result<(), String> {
        let (item, print) = expected
            .iter()
            .find(|(i, _)| i.plist == path)
            .ok_or_else(|| CHANGED_AT_MOVE.to_string())?;
        if !print.still(path) {
            return Err(CHANGED_AT_MOVE.to_string());
        }
        let label = item
            .label
            .as_deref()
            .ok_or_else(|| CHANGED_AT_MOVE.to_string())?;
        verify_on_disk(item, label).map(|_| ())
    };
    let mut outcome = match remove::trash_items_checked(
        env,
        &batch,
        &removables(items),
        &picked,
        dry_run,
        ledger,
        Some(&recheck),
    ) {
        Ok(outcome) => outcome,
        Err(ExecuteError::Ledger { error, mut partial }) => {
            reload_unmoved(ctl, &domain, &unloaded, &mut partial);
            return Err(ExecuteError::Ledger { error, partial });
        }
        Err(other) => {
            reload_unmoved(ctl, &domain, &unloaded, &mut RemovalOutcome::default());
            return Err(other);
        }
    };
    reload_unmoved(ctl, &domain, &unloaded, &mut outcome);
    refused.append(&mut outcome.refused);
    outcome.refused = refused;
    Ok(outcome)
}

/// bootout 之后没移进废纸篓的，重新 `bootstrap` 回去，免得任务停了、文件却还在。
/// 装不回去就在它的拒绝 / 失败原因后面注明 `UNLOADED_NOT_MOVED`；两边都没有它就记进 `failed`。
fn reload_unmoved(
    ctl: &dyn Launchctl,
    domain: &str,
    unloaded: &[(&LaunchItem, &str)],
    outcome: &mut RemovalOutcome,
) {
    for (item, plist) in unloaded {
        if outcome
            .moved
            .iter()
            .any(|(original, _)| original == &item.plist)
        {
            continue;
        }
        let Err(e) = ctl.run(&["bootstrap", domain, plist]) else {
            continue;
        };
        let note = format!("{UNLOADED_NOT_MOVED}: reloading failed: {e:#}");
        match outcome
            .refused
            .iter_mut()
            .chain(outcome.failed.iter_mut())
            .find(|(path, _)| path == &item.plist)
        {
            Some((_, why)) => {
                why.push_str("; ");
                why.push_str(&note);
            }
            None => outcome.failed.push((item.plist.clone(), note)),
        }
    }
}

/// 给 `remove::trash_items` 的候选清单。
pub fn removables(items: &[LaunchItem]) -> Vec<Removable> {
    items
        .iter()
        .map(|i| Removable {
            path: i.plist.clone(),
            size: crate::Size::Unknown,
            hold_back: i.hold_back.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::testing::{scratch, PutBack};
    use crate::{AppSource, ProtectReason, Size};
    use std::fs;
    use std::sync::Mutex;

    /// 每次 `run` 时先调用的钩子（模拟 launchctl 动手期间盘上发生的事）。
    type OnRun = Box<dyn Fn(&[&str]) + Send + Sync>;
    /// 盘上换掉的 plist：说明文字 + 写文件的动作。
    type Swap<'a> = (&'a str, Box<dyn Fn() + 'a>);

    /// 假的 launchctl：按给定的表回答查询，把每次调用记成一行字符串。
    #[derive(Default)]
    struct Fake {
        loaded: Option<HashMap<String, Option<u32>>>,
        gui_disabled: Option<HashMap<String, bool>>,
        system_disabled: Option<HashMap<String, bool>>,
        /// 这些子命令返回失败。
        failing: Vec<&'static str>,
        /// 这些完整的调用（`args.join(" ")`）返回失败。
        failing_calls: Vec<String>,
        /// 每次 `run` 时先调用（模拟 launchctl 动手期间盘上发生的事）。
        on_run: Option<OnRun>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Launchctl for Fake {
        fn user_list(&self) -> Option<HashMap<String, Option<u32>>> {
            self.calls.lock().unwrap().push("list".into());
            self.loaded.clone()
        }
        fn disabled(&self, target: &str) -> Option<HashMap<String, bool>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("print-disabled {target}"));
            if target == "system" {
                self.system_disabled.clone()
            } else {
                self.gui_disabled.clone()
            }
        }
        fn run(&self, args: &[&str]) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(args.join(" "));
            if let Some(hook) = &self.on_run {
                hook(args);
            }
            if self.failing.contains(&args[0]) || self.failing_calls.contains(&args.join(" ")) {
                anyhow::bail!("{} failed", args[0]);
            }
            Ok(())
        }
    }

    fn write_plist(path: &Path, entries: Vec<(&str, plist::Value)>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut dict = plist::Dictionary::new();
        for (k, v) in entries {
            dict.insert(k.into(), v);
        }
        plist::Value::Dictionary(dict).to_file_xml(path).unwrap();
    }

    fn agent(env: &Env, label: &str, program: &str) -> PathBuf {
        let path = env.user_library.join(format!("LaunchAgents/{label}.plist"));
        write_plist(
            &path,
            vec![("Label", label.into()), ("Program", program.into())],
        );
        path
    }

    fn installed(path: PathBuf, name: &str, id: &str, embedded: &[&str]) -> InstalledApp {
        InstalledApp {
            path,
            name: name.into(),
            bundle_id: Some(id.into()),
            version: None,
            executable: None,
            team_id: None,
            embedded_ids: embedded.iter().map(|s| s.to_string()).collect(),
            app_groups: Vec::new(),
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        }
    }

    fn by_label<'a>(items: &'a [LaunchItem], label: &str) -> &'a LaunchItem {
        items
            .iter()
            .find(|i| i.label.as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no item {label}"))
    }

    fn manageable_item(plist: PathBuf, label: &str, program: &str) -> LaunchItem {
        LaunchItem {
            plist,
            domain: LaunchDomain::UserAgent,
            label: Some(label.into()),
            program: Some(program.into()),
            program_exists: false,
            run_at_load: false,
            keep_alive: false,
            owner: None,
            orphan: false,
            status: LaunchStatus::NotLoaded,
            hold_back: None,
        }
    }

    #[test]
    fn parses_programs_flags_and_lists_broken_or_huge_files() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let agents = env.user_library.join("LaunchAgents");
        write_plist(
            &agents.join("a.plist"),
            vec![
                ("Label", "com.x.a".into()),
                ("Program", "/opt/x/a".into()),
                (
                    "ProgramArguments",
                    plist::Value::Array(vec!["/ignored".into()]),
                ),
                ("RunAtLoad", true.into()),
                ("KeepAlive", true.into()),
            ],
        );
        let mut keep = plist::Dictionary::new();
        keep.insert("SuccessfulExit".into(), false.into());
        write_plist(
            &agents.join("b.plist"),
            vec![
                ("Label", "com.x.b".into()),
                (
                    "ProgramArguments",
                    plist::Value::Array(vec!["/opt/x/b".into(), "--flag".into()]),
                ),
                ("KeepAlive", plist::Value::Dictionary(keep)),
            ],
        );
        write_plist(
            &agents.join("c.plist"),
            vec![
                ("Label", "com.x.c".into()),
                ("Program", "/opt/x/c".into()),
                ("KeepAlive", false.into()),
            ],
        );
        fs::write(agents.join("broken.plist"), b"<plist><dict><key>Lab").unwrap();
        fs::write(agents.join("huge.plist"), vec![b' '; 1_100_000]).unwrap();
        fs::write(agents.join("notes.txt"), b"not a job").unwrap();
        fs::create_dir_all(agents.join("folder.plist")).unwrap();

        let fake = Fake::default();
        let items = list_with(&env, &[], &fake);
        assert_eq!(items.len(), 5, "{items:#?}");

        let a = by_label(&items, "com.x.a");
        assert_eq!(a.program.as_deref(), Some(Path::new("/opt/x/a")));
        assert!(a.run_at_load && a.keep_alive);
        let b = by_label(&items, "com.x.b");
        assert_eq!(b.program.as_deref(), Some(Path::new("/opt/x/b")));
        assert!(!b.run_at_load && b.keep_alive, "a KeepAlive dict counts");
        let c = by_label(&items, "com.x.c");
        assert!(!c.keep_alive);

        for name in ["broken.plist", "huge.plist"] {
            let item = items.iter().find(|i| i.plist.ends_with(name)).unwrap();
            assert_eq!(item.label, None);
            assert_eq!(item.program, None);
            assert_eq!(item.status, LaunchStatus::Unknown);
            assert!(!item.orphan, "unknown program is not an orphan");
            assert!(!item.manageable());
        }
    }

    #[test]
    fn owners_come_from_the_program_or_the_label() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let foo_path = crate::apps::testing::make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        let foo = installed(
            foo_path.clone(),
            "Foo",
            "com.acme.foo",
            &["net.helper.thing"],
        );
        let gone = dir.path().join("gone/bin");
        let outside = dir.path().join("opt/tool");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, b"#!/bin/sh\n").unwrap();
        // 包外的符号链接指进包里：按真实位置认。
        let link = dir.path().join("opt/foo-link");
        std::os::unix::fs::symlink(foo_path.join("Contents/MacOS/Foo"), &link).unwrap();

        let inside = foo_path.join("Contents/MacOS/Foo");
        agent(&env, "com.other.runs-foo", &inside.display().to_string());
        agent(&env, "com.other.linked", &link.display().to_string());
        agent(&env, "com.acme.foo.updater", &gone.display().to_string());
        agent(&env, "net.helper.thing", &gone.display().to_string());
        agent(&env, "com.acme.foobar", &gone.display().to_string());
        agent(&env, "com.gone.agent", &gone.display().to_string());
        agent(&env, "com.alive.tool", &outside.display().to_string());
        write_plist(
            &env.user_library.join("LaunchAgents/com.shell.plist"),
            vec![
                ("Label", "com.shell".into()),
                (
                    "ProgramArguments",
                    plist::Value::Array(vec!["sh".into(), "-c".into(), "true".into()]),
                ),
            ],
        );

        let items = list_with(&env, std::slice::from_ref(&foo), &Fake::default());
        let owner = |label: &str| {
            by_label(&items, label)
                .owner
                .as_ref()
                .map(|o| o.name.clone())
        };
        assert_eq!(owner("com.other.runs-foo"), Some("Foo".into()));
        assert_eq!(owner("com.other.linked"), Some("Foo".into()));
        assert_eq!(owner("com.acme.foo.updater"), Some("Foo".into()));
        assert_eq!(owner("net.helper.thing"), Some("Foo".into()), "embedded id");
        assert_eq!(owner("com.acme.foobar"), None, "prefix must end at a dot");
        assert_eq!(owner("com.gone.agent"), None);

        assert!(by_label(&items, "com.other.runs-foo").program_exists);
        assert!(!by_label(&items, "com.acme.foo.updater").orphan, "owned");
        assert!(by_label(&items, "com.acme.foobar").orphan);
        assert!(by_label(&items, "com.gone.agent").orphan);
        assert!(
            !by_label(&items, "com.alive.tool").orphan,
            "program is there"
        );
        let shell = by_label(&items, "com.shell");
        assert!(
            shell.program_exists && !shell.orphan,
            "bare names use launchd's PATH"
        );

        // 孤儿排最前。
        let orphans: Vec<_> = items.iter().take_while(|i| i.orphan).collect();
        assert_eq!(orphans.len(), 2);
    }

    #[test]
    fn hold_backs_for_apple_admin_and_protected() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        agent(&env, "com.apple.something", "/usr/libexec/x");
        agent(&env, "com.mine.ok", "/usr/libexec/x");
        // 开尔文符号 K 会被 APFS 折叠成 K：解析不了就按保护处理。
        agent(&env, "com.\u{212A}ey.agent", "/usr/libexec/x");
        write_plist(
            &env.system_library
                .join("LaunchAgents/com.vendor.agent.plist"),
            vec![("Label", "com.vendor.agent".into())],
        );
        write_plist(
            &env.system_library
                .join("LaunchDaemons/com.vendor.daemon.plist"),
            vec![("Label", "com.vendor.daemon".into())],
        );

        let items = list_with(&env, &[], &Fake::default());
        let hb = |label: &str| by_label(&items, label).hold_back.clone();
        assert_eq!(hb("com.apple.something"), Some(HoldBack::SystemOwned));
        assert_eq!(hb("com.mine.ok"), None);
        assert_eq!(
            hb("com.\u{212A}ey.agent"),
            Some(HoldBack::Protected {
                why: ProtectReason::Unresolvable
            })
        );
        assert_eq!(hb("com.vendor.agent"), Some(HoldBack::NeedsAdmin));
        assert_eq!(hb("com.vendor.daemon"), Some(HoldBack::NeedsAdmin));
        assert!(by_label(&items, "com.mine.ok").manageable());
        assert!(!by_label(&items, "com.vendor.agent").manageable());

        // 按域排：用户域在前，系统后台在最后。
        let domains: Vec<_> = items.iter().map(|i| i.domain).collect();
        let mut sorted = domains.clone();
        sorted.sort();
        assert_eq!(domains, sorted);
    }

    #[test]
    fn statuses_come_from_one_batch_of_queries() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        for label in ["run", "idle", "off", "gone"] {
            agent(&env, label, "/usr/bin/true");
        }
        write_plist(
            &env.system_library.join("LaunchAgents/global.plist"),
            vec![("Label", "global".into())],
        );
        for label in ["d-off", "d-on"] {
            write_plist(
                &env.system_library
                    .join(format!("LaunchDaemons/{label}.plist")),
                vec![("Label", label.into())],
            );
        }
        fs::write(env.user_library.join("LaunchAgents/broken.plist"), b"x").unwrap();

        let fake = Fake {
            loaded: Some(HashMap::from([
                ("run".into(), Some(42)),
                ("idle".into(), None),
                ("off".into(), Some(7)),
                ("global".into(), None),
            ])),
            gui_disabled: Some(HashMap::from([
                ("off".into(), true),
                ("idle".into(), false),
            ])),
            system_disabled: Some(HashMap::from([("d-off".into(), true)])),
            ..Default::default()
        };
        let items = list_with(&env, &[], &fake);
        let st = |label: &str| by_label(&items, label).status;
        assert_eq!(st("run"), LaunchStatus::Loaded { pid: Some(42) });
        assert_eq!(st("idle"), LaunchStatus::Loaded { pid: None });
        assert_eq!(st("off"), LaunchStatus::Disabled, "the switch wins");
        assert_eq!(st("gone"), LaunchStatus::NotLoaded);
        assert_eq!(st("global"), LaunchStatus::Loaded { pid: None });
        assert_eq!(st("d-off"), LaunchStatus::Disabled);
        assert_eq!(
            st("d-on"),
            LaunchStatus::Unknown,
            "system domain is not visible"
        );
        let broken = items.iter().find(|i| i.label.is_none()).unwrap();
        assert_eq!(broken.status, LaunchStatus::Unknown);
        assert_eq!(
            fake.calls(),
            vec![
                "list".to_string(),
                format!("print-disabled {}", gui_domain()),
                "print-disabled system".to_string(),
            ]
        );

        // launchctl 什么都答不上来：都是未知，而不是"未加载"。
        let silent = list_with(&env, &[], &Fake::default());
        assert!(silent.iter().all(|i| i.status == LaunchStatus::Unknown));
    }

    #[test]
    fn nothing_to_list_asks_launchctl_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let fake = Fake::default();
        assert!(list_with(&env, &[], &fake).is_empty());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn parses_launchctl_output() {
        let list = "PID\tStatus\tLabel\n-\t0\tcom.a\n2800\t0\tcom.b\n-\t-9\tcom.c with space\n";
        let parsed = parse_list(list);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed["com.a"], None);
        assert_eq!(parsed["com.b"], Some(2800));
        assert_eq!(parsed["com.c with space"], None);

        let disabled = "\n\tdisabled services = {\n\t\t\"com.a\" => enabled\n\t\t\"com.b\" => disabled\n\t\t\"com.old\" => true\n\t\t\"com.new\" => false\n\t}\n";
        let parsed = parse_disabled(disabled);
        assert_eq!(parsed.len(), 4);
        assert!(!parsed["com.a"] && parsed["com.b"] && parsed["com.old"] && !parsed["com.new"]);
    }

    #[test]
    fn set_enabled_runs_launchctl_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/usr/bin/true");
        let item = manageable_item(plist.clone(), "com.mine.job", "/usr/bin/true");
        let domain = gui_domain();
        let p = plist.display().to_string();

        // 停用：bootout 失败（本来就没加载）不算错。
        let fake = Fake {
            failing: vec!["bootout"],
            ..Default::default()
        };
        set_enabled_with(&env, &item, false, &fake).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                format!("disable {domain}/com.mine.job"),
                format!("bootout {domain} {p}")
            ]
        );

        // 启用：没加载就 bootstrap。
        let fake = Fake {
            loaded: Some(HashMap::new()),
            ..Default::default()
        };
        set_enabled_with(&env, &item, true, &fake).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                format!("enable {domain}/com.mine.job"),
                "list".to_string(),
                format!("bootstrap {domain} {p}")
            ]
        );

        // 已经加载着：不再 bootstrap。
        let fake = Fake {
            loaded: Some(HashMap::from([("com.mine.job".into(), Some(9))])),
            ..Default::default()
        };
        set_enabled_with(&env, &item, true, &fake).unwrap();
        assert_eq!(fake.calls().len(), 2);

        // disable 本身失败：报错，也不再 bootout。
        let fake = Fake {
            failing: vec!["disable"],
            ..Default::default()
        };
        assert!(set_enabled_with(&env, &item, false, &fake).is_err());
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn unmanageable_items_never_reach_launchctl() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/usr/bin/true");
        let ok = manageable_item(plist.clone(), "com.mine.job", "/usr/bin/true");

        let global = LaunchItem {
            domain: LaunchDomain::GlobalAgent,
            hold_back: Some(HoldBack::NeedsAdmin),
            ..ok.clone()
        };
        let apple = LaunchItem {
            hold_back: Some(HoldBack::SystemOwned),
            ..ok.clone()
        };
        let nameless = LaunchItem {
            label: None,
            ..ok.clone()
        };
        // 字段被改成"可管理"、位置却不在 ~/Library/LaunchAgents：照样拒绝。
        let forged = LaunchItem {
            plist: env.system_library.join("LaunchDaemons/com.mine.job.plist"),
            ..ok.clone()
        };
        let slash = LaunchItem {
            label: Some("x/../../system/y".into()),
            ..ok.clone()
        };
        let apple_label = LaunchItem {
            label: Some("com.apple.sneaky".into()),
            ..ok.clone()
        };
        for item in [&global, &apple, &nameless, &forged, &slash, &apple_label] {
            let fake = Fake::default();
            for enabled in [true, false] {
                assert!(
                    set_enabled_with(&env, item, enabled, &fake).is_err(),
                    "{item:?}"
                );
            }
            assert!(fake.calls().is_empty(), "{item:?}");
        }
    }

    #[test]
    fn remove_boots_out_trashes_records_and_restores() {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/nowhere/job");
        let all = list_with(&env, &[], &Fake::default());
        assert_eq!(all.len(), 1);
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [plist.clone()].into();

        let fake = Fake {
            failing: vec!["bootout"],
            ..Default::default()
        };
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        let mut guard = PutBack::new();
        guard
            .items
            .extend(outcome.moved.iter().map(|(o, t)| (t.clone(), o.clone())));

        assert_eq!(
            fake.calls(),
            vec![format!("bootout {} {}", gui_domain(), plist.display())]
        );
        assert_eq!(outcome.moved.len(), 1);
        assert!(!plist.exists());
        let records = ledger.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, RecordKind::LaunchItems);
        assert_eq!(records[0].app_name, "com.mine.job");
        assert_eq!(records[0].items[0].original, plist);

        let restored = ledger
            .restore(&env, outcome.record_id.as_deref().unwrap())
            .unwrap();
        assert_eq!(restored.restored, vec![plist.clone()]);
        assert!(plist.is_file(), "the plist is back where it was");
    }

    #[test]
    fn remove_refuses_what_it_cannot_manage() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let a = agent(&env, "com.mine.a", "/nowhere/a");
        let b = agent(&env, "com.mine.b", "/nowhere/b");
        write_plist(
            &env.system_library.join("LaunchAgents/com.vendor.plist"),
            vec![("Label", "com.vendor".into())],
        );
        let all = list_with(&env, &[], &Fake::default());
        assert_eq!(all.len(), 3);
        let selected: HashSet<PathBuf> = all.iter().map(|i| i.plist.clone()).collect();

        let fake = Fake::default();
        let outcome = remove_with(&env, &all, &selected, Source::App, true, None, &fake).unwrap();
        assert!(fake.calls().is_empty(), "dry run never calls launchctl");
        assert!(outcome.dry_run);
        let moved: HashSet<PathBuf> = outcome.moved.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(moved, [a.clone(), b.clone()].into());
        assert_eq!(outcome.refused.len(), 1);
        assert!(outcome.refused[0].0.ends_with("com.vendor.plist"));
        assert!(a.exists() && b.exists());

        // 只选了不能动的：一个都不动，也不问 launchctl。
        let vendor: HashSet<PathBuf> = [outcome.refused[0].0.clone()].into();
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let none = remove_with(
            &env,
            &all,
            &vendor,
            Source::App,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        assert!(none.moved.is_empty() && none.record_id.is_none());
        assert!(fake.calls().is_empty());
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn several_items_make_one_neutral_batch() {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let a = agent(&env, "com.mine.a", "/nowhere/a");
        let b = agent(&env, "com.mine.b", "/nowhere/b");
        let all = list_with(&env, &[], &Fake::default());
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [a.clone(), b.clone()].into();
        let fake = Fake::default();
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::App,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        let mut guard = PutBack::new();
        guard
            .items
            .extend(outcome.moved.iter().map(|(o, t)| (t.clone(), o.clone())));
        assert_eq!(outcome.moved.len(), 2);
        assert_eq!(fake.calls().len(), 2, "one bootout each");
        let records = ledger.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].app_name, "2 launch items");
        assert_eq!(records[0].bundle_id, None);
        assert_eq!(records[0].kind, RecordKind::LaunchItems);
        let restored = ledger.restore(&env, &records[0].id).unwrap();
        assert_eq!(restored.restored.len(), 2);
        assert!(a.is_file() && b.is_file());
    }

    #[test]
    fn remove_checks_the_request_before_booting_anything_out() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let a = agent(&env, "com.mine.a", "/nowhere/a");
        let all = list_with(&env, &[], &Fake::default());
        let fake = Fake::default();
        let selected: HashSet<PathBuf> = [a.clone()].into();
        assert!(matches!(
            remove_with(&env, &all, &selected, Source::Cli, false, None, &fake),
            Err(ExecuteError::LedgerRequired)
        ));
        let stranger: HashSet<PathBuf> = [dir.path().join("x.plist")].into();
        assert!(matches!(
            remove_with(&env, &all, &stranger, Source::Cli, true, None, &fake),
            Err(ExecuteError::NotInPlan(_))
        ));
        assert!(matches!(
            remove_with(&env, &all, &HashSet::new(), Source::Cli, true, None, &fake),
            Err(ExecuteError::NothingSelected)
        ));
        assert!(fake.calls().is_empty());
        assert!(a.exists());
    }

    #[test]
    fn labels_are_read_verbatim_and_odd_ones_are_not_manageable() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let spaced = agent(&env, " com.mine.spaced", "/usr/bin/true");
        let inner = env.user_library.join("LaunchAgents/inner.plist");
        write_plist(
            &inner,
            vec![
                ("Label", "com.mine.in ner".into()),
                ("Program", "/usr/bin/true".into()),
            ],
        );
        agent(&env, "com.mine.ok-1_2", "/usr/bin/true");
        let items = list_with(&env, &[], &Fake::default());
        let spaced_item = items.iter().find(|i| i.plist == spaced).unwrap();
        assert_eq!(
            spaced_item.label.as_deref(),
            Some(" com.mine.spaced"),
            "not trimmed"
        );
        assert!(!spaced_item.manageable());
        assert!(!by_label(&items, "com.mine.in ner").manageable());
        assert!(by_label(&items, "com.mine.ok-1_2").manageable());

        // 字段被改成"可管理"也一样拒绝，不碰 launchctl。
        for item in [spaced_item, by_label(&items, "com.mine.in ner")] {
            let forged = LaunchItem {
                hold_back: None,
                ..item.clone()
            };
            let fake = Fake::default();
            assert!(set_enabled_with(&env, &forged, false, &fake).is_err());
            assert!(fake.calls().is_empty());
        }
    }

    #[test]
    fn plists_changed_after_the_scan_never_reach_launchctl() {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/usr/bin/true");
        let scanned = list_with(&env, &[], &Fake::default());
        let item = by_label(&scanned, "com.mine.job").clone();
        assert!(item.manageable());
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [plist.clone()].into();
        let elsewhere = dir.path().join("elsewhere.plist");

        let swaps: Vec<Swap<'_>> = vec![
            (
                "symlink to a twin",
                Box::new(|| {
                    write_plist(
                        &elsewhere,
                        vec![
                            ("Label", "com.mine.job".into()),
                            ("Program", "/usr/bin/true".into()),
                        ],
                    );
                    fs::remove_file(&plist).unwrap();
                    std::os::unix::fs::symlink(&elsewhere, &plist).unwrap();
                }),
            ),
            (
                "other label",
                Box::new(|| {
                    write_plist(
                        &plist,
                        vec![
                            ("Label", "com.mine.other".into()),
                            ("Program", "/usr/bin/true".into()),
                        ],
                    )
                }),
            ),
            (
                "label with a space",
                Box::new(|| {
                    write_plist(
                        &plist,
                        vec![
                            ("Label", "com.mine.job ".into()),
                            ("Program", "/usr/bin/true".into()),
                        ],
                    )
                }),
            ),
            (
                "other program",
                Box::new(|| {
                    write_plist(
                        &plist,
                        vec![
                            ("Label", "com.mine.job".into()),
                            ("Program", "/tmp/evil".into()),
                        ],
                    )
                }),
            ),
            (
                "writable by others",
                Box::new(|| {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&plist, fs::Permissions::from_mode(0o666)).unwrap();
                }),
            ),
            (
                "not a plist any more",
                Box::new(|| fs::write(&plist, b"garbage").unwrap()),
            ),
        ];
        for (what, swap) in swaps {
            let _ = fs::remove_file(&plist);
            agent(&env, "com.mine.job", "/usr/bin/true");
            swap();
            let fake = Fake::default();
            for enabled in [true, false] {
                assert!(
                    set_enabled_with(&env, &item, enabled, &fake).is_err(),
                    "{what}"
                );
            }
            let outcome = remove_with(
                &env,
                std::slice::from_ref(&item),
                &selected,
                Source::Cli,
                false,
                Some(&ledger),
                &fake,
            )
            .unwrap();
            assert!(outcome.moved.is_empty(), "{what}");
            assert_eq!(outcome.refused.len(), 1, "{what}");
            assert!(fake.calls().is_empty(), "{what}: {:?}", fake.calls());
        }
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn a_symlinked_launch_agents_folder_hides_the_user_domain() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let global = env.system_library.join("LaunchAgents");
        write_plist(
            &global.join("com.vendor.agent.plist"),
            vec![
                ("Label", "com.vendor.agent".into()),
                ("Program", "/usr/bin/true".into()),
            ],
        );
        fs::create_dir_all(&env.user_library).unwrap();
        let agents = env.user_library.join("LaunchAgents");
        std::os::unix::fs::symlink(&global, &agents).unwrap();

        let items = list_with(&env, &[], &Fake::default());
        assert_eq!(items.len(), 1, "{items:#?}");
        assert_eq!(items[0].domain, LaunchDomain::GlobalAgent);
        assert!(!items[0].manageable());
        // 拼一个"用户域"的项指进去：照样拒绝。
        let forged = manageable_item(
            agents.join("com.vendor.agent.plist"),
            "com.vendor.agent",
            "/usr/bin/true",
        );
        let fake = Fake::default();
        assert!(set_enabled_with(&env, &forged, false, &fake).is_err());
        assert!(fake.calls().is_empty());

        // `~/Library` 本身是链接也一样。
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let real_library = dir.path().join("real-library");
        write_plist(
            &real_library.join("LaunchAgents/com.mine.job.plist"),
            vec![("Label", "com.mine.job".into())],
        );
        fs::create_dir_all(&env.home).unwrap();
        std::os::unix::fs::symlink(&real_library, &env.user_library).unwrap();
        assert!(list_with(&env, &[], &Fake::default()).is_empty());
    }

    #[test]
    fn symlinked_plists_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let target = dir.path().join("twin.plist");
        write_plist(&target, vec![("Label", "com.mine.twin".into())]);
        let agents = env.user_library.join("LaunchAgents");
        fs::create_dir_all(&agents).unwrap();
        std::os::unix::fs::symlink(&target, agents.join("com.mine.twin.plist")).unwrap();
        agent(&env, "com.mine.real", "/usr/bin/true");
        let items = list_with(&env, &[], &Fake::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label.as_deref(), Some("com.mine.real"));
    }

    #[test]
    fn programs_that_cannot_be_looked_at_are_not_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        agent(&env, "com.mine.external", "/Volumes/Ext/tool");
        let locked = dir.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        let hidden = locked.join("tool");
        fs::write(&hidden, b"#!/bin/sh\n").unwrap();
        agent(&env, "com.mine.hidden", &hidden.display().to_string());
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let items = list_with(&env, &[], &Fake::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let external = by_label(&items, "com.mine.external");
        assert!(external.program_exists && !external.orphan);
        let hidden = by_label(&items, "com.mine.hidden");
        assert!(
            hidden.program_exists && !hidden.orphan,
            "EACCES is not gone"
        );
    }

    #[test]
    fn remove_reloads_what_it_unloaded_but_could_not_move() {
        // History 写不了：bootout 成功的（a）要装回去；本来没加载、bootout 失败的（b）不去加载它。
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let a = agent(&env, "com.mine.a", "/nowhere/a");
        let b = agent(&env, "com.mine.b", "/nowhere/b");
        let all = list_with(&env, &[], &Fake::default());
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, b"").unwrap();
        let broken = Ledger::at(blocker.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [a.clone(), b.clone()].into();
        let domain = gui_domain();
        let fake = Fake {
            failing_calls: vec![format!("bootout {domain} {}", b.display())],
            ..Default::default()
        };
        let result = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&broken),
            &fake,
        );
        assert!(matches!(result, Err(ExecuteError::Ledger { .. })));
        let calls = fake.calls();
        assert_eq!(
            calls,
            vec![
                format!("bootout {domain} {}", a.display()),
                format!("bootout {domain} {}", b.display()),
                format!("bootstrap {domain} {}", a.display()),
            ]
        );
        assert!(a.exists() && b.exists());
    }

    #[test]
    fn a_failed_move_is_reloaded_or_reported() {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/nowhere/job");
        let all = list_with(&env, &[], &Fake::default());
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [plist.clone()].into();
        let domain = gui_domain();
        let shown = plist.display().to_string();

        // bootout 期间文件没了：移动失败，装回去也失败 → 失败原因里注明"卸下了但没移走"。
        let gone = plist.clone();
        let fake = Fake {
            failing: vec!["bootstrap"],
            on_run: Some(Box::new(move |args: &[&str]| {
                if args[0] == "bootout" {
                    let _ = fs::remove_file(&gone);
                }
            })),
            ..Default::default()
        };
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(
            fake.calls(),
            vec![
                format!("bootout {domain} {shown}"),
                format!("bootstrap {domain} {shown}")
            ]
        );
        let (path, why) = outcome
            .failed
            .iter()
            .chain(&outcome.refused)
            .find(|(p, _)| p == &plist)
            .expect("the item is reported");
        assert_eq!(path, &plist);
        assert!(why.contains(UNLOADED_NOT_MOVED), "{why}");

        // 本来就没加载（bootout 失败）：移不走也不去加载它。
        agent(&env, "com.mine.job", "/nowhere/job");
        let gone = plist.clone();
        let fake = Fake {
            failing: vec!["bootout"],
            on_run: Some(Box::new(move |args: &[&str]| {
                if args[0] == "bootout" {
                    let _ = fs::remove_file(&gone);
                }
            })),
            ..Default::default()
        };
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(fake.calls(), vec![format!("bootout {domain} {shown}")]);
    }

    #[test]
    fn a_plist_swapped_during_bootout_is_not_moved() {
        // 核对过、`bootout` 正在跑的时候把盘上那份换成别的（同一用户能写 `~/Library/LaunchAgents`）：
        // 换成什么都不能移——移走的必须始终是核对过的那一份。
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/nowhere/job");
        let all = list_with(&env, &[], &Fake::default());
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [plist.clone()].into();
        let domain = gui_domain();
        let swapped = plist.clone();
        let fake = Fake {
            on_run: Some(Box::new(move |args: &[&str]| {
                if args[0] == "bootout" {
                    write_plist(
                        &swapped,
                        vec![
                            ("Label", "com.apple.sneaky".into()),
                            ("Program", "/usr/bin/true".into()),
                        ],
                    );
                }
            })),
            ..Default::default()
        };
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        assert!(outcome.moved.is_empty());
        // 卸下来了就装回去：调用序列里 bootout 后面跟着 bootstrap。
        assert_eq!(
            fake.calls(),
            vec![
                format!("bootout {domain} {}", plist.display()),
                format!("bootstrap {domain} {}", plist.display()),
            ]
        );
        let (_, why) = outcome
            .refused
            .iter()
            .find(|(p, _)| p == &plist)
            .expect("the swap is reported");
        assert!(why.contains(CHANGED_AT_MOVE), "{why}");
        assert!(plist.exists(), "the swapped file stays where it is");
        assert!(ledger.list().unwrap().is_empty(), "nothing was recorded");
    }

    #[test]
    fn an_in_place_rewrite_cannot_slip_past_the_fingerprint() {
        // 同一个 inode 原地改写、把 mtime 设回去：只比元数据看不出来，所以指纹里有内容的哈希和 ctime。
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = agent(&env, "com.mine.job", "/usr/bin/true");
        let before = fs::read(&plist).unwrap();
        let stamp = fs::metadata(&plist).unwrap().modified().unwrap();
        let all = list_with(&env, &[], &Fake::default());
        let ledger = Ledger::at(env.data_dir.join("history.jsonl"));
        let selected: HashSet<PathBuf> = [plist.clone()].into();
        let domain = gui_domain();
        let swapped = plist.clone();
        let fake = Fake {
            on_run: Some(Box::new(move |args: &[&str]| {
                if args[0] == "bootout" {
                    let at = before
                        .windows(12)
                        .position(|w| w == b"com.mine.job")
                        .expect("the label is in there");
                    let mut bytes = before.clone();
                    bytes[at..at + 12].copy_from_slice(b"com.apple.jb");
                    fs::write(&swapped, &bytes).unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&swapped)
                        .unwrap()
                        .set_modified(stamp)
                        .unwrap();
                }
            })),
            ..Default::default()
        };
        let outcome = remove_with(
            &env,
            &all,
            &selected,
            Source::Cli,
            false,
            Some(&ledger),
            &fake,
        )
        .unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(
            fake.calls(),
            vec![
                format!("bootout {domain} {}", plist.display()),
                format!("bootstrap {domain} {}", plist.display()),
            ]
        );
        let (_, why) = outcome
            .refused
            .iter()
            .find(|(p, _)| p == &plist)
            .expect("the rewrite is reported");
        assert!(why.contains(CHANGED_AT_MOVE), "{why}");
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn a_deep_plist_bomb_is_refused_instead_of_crashing() {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        let plist = env.user_library.join("LaunchAgents/com.mine.job.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        // 60 000 层嵌套、约 700 KB：不到 `PLIST_LIMIT`，但足以把直接建树的解析器撑爆栈。
        let depth = 60_000;
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>",
        );
        xml.push_str("<key>Label</key><string>com.mine.job</string>");
        xml.push_str("<key>Program</key><string>/nowhere/job</string>");
        xml.push_str("<key>Nested</key>");
        xml.push_str(&"<array>".repeat(depth));
        xml.push_str(&"</array>".repeat(depth));
        xml.push_str("</dict></plist>");
        assert!(xml.len() < PLIST_LIMIT as usize, "{} bytes", xml.len());
        fs::write(&plist, xml).unwrap();

        // 列出来：读不了就不认，不可管理。
        let items = list_with(&env, &[], &Fake::default());
        let item = items.iter().find(|i| i.plist == plist).expect("listed");
        assert!(item.label.is_none() && !item.manageable());

        // 动手：核对时就拒绝，碰都不碰 launchctl。
        let fake = Fake::default();
        let handmade = manageable_item(plist.clone(), "com.mine.job", "/nowhere/job");
        let err = set_enabled_with(&env, &handmade, false, &fake).unwrap_err();
        assert!(err.to_string().contains("changed since the scan"), "{err}");
        assert!(fake.calls().is_empty());
    }
}
