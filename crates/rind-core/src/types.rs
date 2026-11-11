//! 全部跨模块共享的数据类型。这里的东西是 CLI、GUI 和各模块之间的契约，
//! 改动字段或签名时要同步 docs/ARCHITECTURE.md，并全工作区编译一遍。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一个 App 的最小引用：用来说明"还有谁在用这个文件"。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppRef {
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: PathBuf,
}

/// App 是从哪儿来的。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppSource {
    AppStore,
    /// Homebrew cask，带 token。
    Homebrew(String),
    Other,
}

/// 已安装的 App，连同匹配残留文件所需的全部证据来源。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledApp {
    pub path: PathBuf,
    /// 显示名：CFBundleDisplayName → CFBundleName → 文件名去掉 `.app`。
    pub name: String,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub executable: Option<PathBuf>,
    /// 签名的 Team ID（`codesign -dv` 的 `TeamIdentifier`）。
    pub team_id: Option<String>,
    /// 包内 Frameworks / PlugIns / XPCServices / Helpers / LoginItems 等的 bundle id。
    pub embedded_ids: Vec<String>,
    /// entitlements 里的 `com.apple.security.application-groups`。
    pub app_groups: Vec<String>,
    pub source: AppSource,
    pub size: Size,
    /// 是否是 Apple 自带（不可卸载，列表里不显示）。
    pub is_system: bool,
}

impl InstalledApp {
    pub fn as_ref(&self) -> AppRef {
        AppRef {
            name: self.name.clone(),
            bundle_id: self.bundle_id.clone(),
            path: self.path.clone(),
        }
    }
}

/// 一个路径占多少空间。未知就是未知，绝不写 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    Known(u64),
    Unknown,
}

impl Size {
    pub fn known(self) -> Option<u64> {
        match self {
            Size::Known(n) => Some(n),
            Size::Unknown => None,
        }
    }
}

/// 把握有多大。顺序有意义：`Possible < Likely < Certain`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Possible,
    Likely,
    Certain,
}

/// 凭什么说这个文件属于这个 App。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EvidenceKind {
    /// 名字里含完整 bundle id。
    BundleId,
    /// 名字里含包内某个组件的 bundle id。
    EmbeddedBundleId,
    /// 名字里含 App Group id。
    AppGroup,
    /// 只匹配到 bundle id 的前缀（同一厂商的别的 id）。
    BundleIdPrefix,
    /// 名字里含 Team ID（如 Group Containers 下的 `TEAMID.com.foo`）。
    TeamId,
    /// 名字和 App 名相同（或整词包含）。
    Name,
    /// launchd plist 运行的程序在 App 包内。
    LaunchdProgram,
    /// Homebrew cask 的 zap / uninstall 段点名了它。
    CaskZap { token: String },
    /// 安装收据（`/private/var/db/receipts`）属于它。
    Receipt,
    /// 规则库命中。
    Rule { rule: String },
    /// 孤儿：没有任何已安装的 App 认领它，`signal` 说明凭什么认定主人已经不在了。
    Orphan { signal: OrphanSignal },
}

/// 认定"主人已经不在了"的依据。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanSignal {
    /// 名字是一个 bundle id，盘上找不到任何 App 用这个 id（也不是它包内的组件）。
    BundleIdGone,
    /// launchd plist 要运行的程序已经不存在了。
    ProgramMissing,
    /// 规则库或 Homebrew cask 点名了它，但对应的 App 没装。
    KnownAppGone,
    /// 只有名字像某个 App 的数据，找不到对应的 App。
    /// 目前不产出（普通名字的陌生文件夹误报太多，一律不报）；留着是为了读得懂旧数据。
    NameGone,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub confidence: Confidence,
    /// 真正匹配上的那个 token，比如 `com.foo.bar` 或 `Foo`。给人看的。
    pub matched: String,
}

/// 这个残留是哪一类，按它所在的位置分。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverKind {
    AppSupport,
    Cache,
    Preferences,
    Container,
    GroupContainer,
    LaunchAgent,
    LaunchDaemon,
    SavedState,
    Log,
    WebData,
    Cookies,
    ApplicationScripts,
    Receipt,
    HomebrewCask,
    HomeDotfile,
    Other,
}

/// 保护区：这些地方无论如何不动。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectReason {
    ICloudDrive,
    Keychain,
    SshKeys,
    GpgKeys,
    Mail,
    Messages,
    Safari,
    Contacts,
    Calendars,
    Notes,
    PhotoLibrary,
    MusicLibrary,
    DeviceBackups,
    /// Desktop / Documents / Downloads 这几个文件夹本身。
    UserFolder,
    /// 家目录、`~/Library` 本身，`/`、`/Applications` 本身等。
    RootFolder,
    /// `/System`、`/usr`、`/bin` 等系统路径。
    SystemPath,
    /// 带已知加密钱包扩展的浏览器 profile。
    Wallet,
    /// Rind 自己的程序和数据。
    RindItself,
    /// 路径解析不了（相对路径、读不到、祖先里有符号链接或大小写对不上），按保护处理。
    Unresolvable,
}

/// 匹配上了，但 Rind 不会替用户选，甚至不会动。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum HoldBack {
    /// 在保护区里。任何选择都不能覆盖。
    Protected { why: ProtectReason },
    /// 另一些已安装的 App 也在用。可以手动选。
    SharedWith { apps: Vec<AppRef> },
    /// 读不了（无权限）。可以手动选，但移动时可能失败。
    Unreadable,
    /// 要管理员权限才能移动（root 所有，或父目录写不了 / 带 sticky 位）。Rind 不提权，只列出、不移动。
    NeedsAdmin,
    /// 是 macOS 自己的东西。不能选。
    SystemOwned,
    /// 它所属的程序正在运行（如浏览器开着时的缓存）。退出后重新扫描才能选。
    InUse { app: String },
    /// 自己能动，但里面有不能动的东西（照片库、钱包、别人在用的……）。整个移走会把它们一起带走，所以不能选。
    Contains { path: PathBuf, why: Box<HoldBack> },
}

impl HoldBack {
    /// 用户能不能手动勾上它。
    pub fn selectable(&self) -> bool {
        matches!(self, HoldBack::SharedWith { .. } | HoldBack::Unreadable)
    }
}

/// 扫描结果里的一项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftoverMatch {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    pub size: Size,
    pub evidence: Evidence,
    /// 其它也声称拥有它的已安装 App。
    pub rivals: Vec<AppRef>,
    pub hold_back: Option<HoldBack>,
}

impl LeftoverMatch {
    /// Rind 会替用户勾上吗：把握够大、没人共用、不被挡、大小已知。
    pub fn suggested(&self) -> bool {
        self.hold_back.is_none()
            && self.rivals.is_empty()
            && self.evidence.confidence >= Confidence::Likely
            && matches!(self.size, Size::Known(_))
    }

    /// 用户能不能勾上它。
    pub fn selectable(&self) -> bool {
        self.hold_back.as_ref().is_none_or(HoldBack::selectable)
    }
}

/// 扫描进度，给界面显示用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanProgress {
    /// 正在看某个位置（给人看的位置名，如 `~/Library/Caches`）。
    Location(String),
    /// 正在度量第 `done` / `total` 项。
    Measuring {
        done: usize,
        total: usize,
    },
    Finished,
}

/// 一次卸载的计划：App 本体 + 全部残留。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallPlan {
    pub app: InstalledApp,
    /// App 本体能不能动。
    pub app_hold_back: Option<HoldBack>,
    pub leftovers: Vec<LeftoverMatch>,
    /// 计划生成时 App 是否在运行。
    pub running: bool,
}

impl UninstallPlan {
    /// Rind 的建议选择。
    pub fn suggested_selection(&self) -> Selection {
        Selection {
            include_app: self.app_hold_back.is_none(),
            paths: self
                .leftovers
                .iter()
                .filter(|m| m.suggested())
                .map(|m| m.path.clone())
                .collect(),
        }
    }

    /// 选中的部分一共能释放多少（只算已知的）。
    pub fn freed_by(&self, selection: &Selection) -> u64 {
        let mut total = 0;
        if selection.include_app {
            total += self.app.size.known().unwrap_or(0);
        }
        for m in &self.leftovers {
            if selection.paths.contains(&m.path) {
                total += m.size.known().unwrap_or(0);
            }
        }
        total
    }
}

/// 用户最终选了什么。移动时只认这里面的路径。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub include_app: bool,
    pub paths: BTreeSet<PathBuf>,
}

/// 谁发起的。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Cli,
    App,
}

/// 一次移动的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalOutcome {
    /// History 记录的 id；dry run 时为 None。
    pub record_id: Option<String>,
    /// (原路径, 废纸篓里的路径)
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// (原路径, 原因)
    pub refused: Vec<(PathBuf, String)>,
    /// (原路径, 错误)
    pub failed: Vec<(PathBuf, String)>,
    /// 释放了多少（只算已知大小）。
    pub freed: u64,
    pub dry_run: bool,
}

/// 一条 History 记录是哪种操作留下的。旧记录没有这个字段，按卸载读。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    #[default]
    Uninstall,
    /// 清理孤儿残留。
    Orphans,
    /// 清理缓存、日志、开发产物。
    Cleanup,
    /// 空间分析里手动选的文件和文件夹。
    Files,
    /// 移除启动项。
    LaunchItems,
}

/// History 里的一条记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub kind: RecordKind,
    /// 卸载时是 App 名；其它操作是给人看的一句标题（如"缓存清理"、某个孤儿的推测名）。
    pub app_name: String,
    pub bundle_id: Option<String>,
    pub source: Source,
    pub items: Vec<HistoryItem>,
    pub restored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub original: PathBuf,
    pub trashed: PathBuf,
    pub size: Size,
    /// 意图已记下、还没移动完成；放回时跳过。
    #[serde(default)]
    pub pending: bool,
    /// 这一项已经放回去了。
    #[serde(default)]
    pub restored: bool,
}

/// 恢复的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub restored: Vec<PathBuf>,
    /// (原路径, 原因)：废纸篓里已经没有了、原位置已被占用、跨卷复制失败……
    pub skipped: Vec<(PathBuf, String)>,
}
