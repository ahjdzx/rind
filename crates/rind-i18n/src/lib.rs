//! 把 rind-core 的结构化结果翻成人话。zh-CN 默认，en 备用。

mod strings;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use rind_core::uninstall::ExecuteError;
use rind_core::{Confidence, Evidence, EvidenceKind, HoldBack, LeftoverKind, ProtectReason, Size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    ZhCn,
    En,
}

impl Lang {
    /// `RIND_LANG=en|zh`，否则看 `LC_ALL` / `LANG`，默认中文。
    pub fn detect() -> Lang {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Lang::from_vars(
            var("RIND_LANG").as_deref(),
            var("LC_ALL").as_deref(),
            var("LANG").as_deref(),
        )
    }

    /// 纯函数，便于测试：`RIND_LANG` 优先；再看 `LC_ALL`、`LANG` 里第一个有值的，含 `zh` 就是中文；
    /// 一个都没有就是中文。
    pub fn from_vars(rind_lang: Option<&str>, lc_all: Option<&str>, lang: Option<&str>) -> Lang {
        if let Some(explicit) = rind_lang {
            let explicit = explicit.trim().to_ascii_lowercase();
            if explicit.starts_with("en") {
                return Lang::En;
            }
            if explicit.starts_with("zh") {
                return Lang::ZhCn;
            }
        }
        match lc_all.or(lang) {
            Some(locale) if locale.to_ascii_lowercase().contains("zh") => Lang::ZhCn,
            Some(_) => Lang::En,
            None => Lang::ZhCn,
        }
    }

    /// `zh-CN` / `en`。
    pub fn tag(self) -> &'static str {
        match self {
            Lang::ZhCn => "zh-CN",
            Lang::En => "en",
        }
    }
}

/// 界面与 CLI 的固定文案，按 key 取。key 不存在时返回 key 本身（便于发现漏翻）。
pub fn t(lang: Lang, key: &str) -> &'static str {
    match strings::TABLE.iter().find(|(k, _, _)| *k == key) {
        Some((_, zh, en)) => pick(lang, zh, en),
        None => missing(key),
    }
}

/// 全部 key，给测试和校对用。
pub fn keys() -> impl Iterator<Item = &'static str> {
    strings::TABLE.iter().map(|(k, _, _)| *k)
}

/// 把 `{n}`、`{size}` 这样的占位符填上。
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

pub fn describe_evidence(lang: Lang, evidence: &Evidence) -> String {
    let m = evidence.matched.as_str();
    match (&evidence.kind, lang) {
        (EvidenceKind::BundleId, Lang::ZhCn) => format!("名字里含它的 bundle id：{m}"),
        (EvidenceKind::BundleId, Lang::En) => format!("Its name holds the app's bundle id: {m}"),
        (EvidenceKind::EmbeddedBundleId, Lang::ZhCn) => format!("名字里含它内置组件的 id：{m}"),
        (EvidenceKind::EmbeddedBundleId, Lang::En) => {
            format!("Its name holds the id of a component inside the app: {m}")
        }
        (EvidenceKind::AppGroup, Lang::ZhCn) => format!("名字里含它的 App Group：{m}"),
        (EvidenceKind::AppGroup, Lang::En) => {
            format!("Its name holds the app's application group: {m}")
        }
        (EvidenceKind::BundleIdPrefix, Lang::ZhCn) => {
            format!("只匹配到 bundle id 的前缀，同一厂商的：{m}")
        }
        (EvidenceKind::BundleIdPrefix, Lang::En) => {
            format!("Only the bundle id's prefix matches, the same maker's: {m}")
        }
        (EvidenceKind::TeamId, Lang::ZhCn) => format!("名字里含它的签名 Team ID：{m}"),
        (EvidenceKind::TeamId, Lang::En) => format!("Its name holds the app's signing team: {m}"),
        (EvidenceKind::Name, Lang::ZhCn) => match evidence.confidence {
            Confidence::Possible => format!("名字里含 App 的名字：{m}"),
            _ => format!("名字和 App 的名字相同：{m}"),
        },
        (EvidenceKind::Name, Lang::En) => match evidence.confidence {
            Confidence::Possible => format!("Its name holds the app's name: {m}"),
            _ => format!("Named like the app: {m}"),
        },
        (EvidenceKind::LaunchdProgram, Lang::ZhCn) => {
            format!("这个启动项运行的是 App 包内的程序：{m}")
        }
        (EvidenceKind::LaunchdProgram, Lang::En) => {
            format!("This launch job runs a program inside the app: {m}")
        }
        (EvidenceKind::CaskZap { token }, Lang::ZhCn) => {
            format!("Homebrew cask「{token}」的 zap 段点名了它")
        }
        (EvidenceKind::CaskZap { token }, Lang::En) => {
            format!("Named by the zap stanza of the Homebrew cask {token}")
        }
        (EvidenceKind::Receipt, Lang::ZhCn) => format!("是它的安装收据：{m}"),
        (EvidenceKind::Receipt, Lang::En) => format!("The app's installer receipt: {m}"),
        (EvidenceKind::Rule { rule }, Lang::ZhCn) => {
            if m.is_empty() {
                format!("规则库「{rule}」点名了它")
            } else {
                format!("规则库「{rule}」点名了它：{m}")
            }
        }
        (EvidenceKind::Rule { rule }, Lang::En) => {
            if m.is_empty() {
                format!("Named by the rule {rule}")
            } else {
                format!("Named by the rule {rule}: {m}")
            }
        }
    }
}

pub fn describe_hold_back(lang: Lang, hold_back: &HoldBack) -> String {
    match (hold_back, lang) {
        (HoldBack::Protected { why }, Lang::ZhCn) => {
            format!("受保护：{}", describe_protect(lang, *why))
        }
        (HoldBack::Protected { why }, Lang::En) => {
            format!("Protected: {}", describe_protect(lang, *why))
        }
        (HoldBack::SharedWith { apps }, _) => {
            let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
            match lang {
                Lang::ZhCn => format!("另外 {} 个 App 也在用：{}", names.len(), names.join("、")),
                Lang::En => match names.len() {
                    1 => format!("Another app uses it too: {}", names[0]),
                    n => format!("{n} other apps use it too: {}", names.join(", ")),
                },
            }
        }
        (HoldBack::Unreadable, Lang::ZhCn) => "读不了，没有权限".to_string(),
        (HoldBack::Unreadable, Lang::En) => "Can't be read, no permission".to_string(),
        (HoldBack::NeedsAdmin, Lang::ZhCn) => "需要管理员权限，这一版先不动它".to_string(),
        (HoldBack::NeedsAdmin, Lang::En) => {
            "Needs an administrator; left alone for now".to_string()
        }
        (HoldBack::SystemOwned, Lang::ZhCn) => "是 macOS 自己的".to_string(),
        (HoldBack::SystemOwned, Lang::En) => "Belongs to macOS".to_string(),
    }
}

pub fn describe_kind(lang: Lang, kind: LeftoverKind) -> &'static str {
    let (zh, en) = match kind {
        LeftoverKind::AppSupport => ("应用支持", "Application Support"),
        LeftoverKind::Cache => ("缓存", "Cache"),
        LeftoverKind::Preferences => ("偏好设置", "Preferences"),
        LeftoverKind::Container => ("沙盒容器", "Container"),
        LeftoverKind::GroupContainer => ("共享容器", "Group container"),
        LeftoverKind::LaunchAgent => ("登录启动项", "Launch agent"),
        LeftoverKind::LaunchDaemon => ("系统守护进程", "Launch daemon"),
        LeftoverKind::SavedState => ("窗口状态", "Saved state"),
        LeftoverKind::Log => ("日志", "Logs"),
        LeftoverKind::WebData => ("网页数据", "Web data"),
        LeftoverKind::Cookies => ("Cookies", "Cookies"),
        LeftoverKind::ApplicationScripts => ("应用脚本", "Application scripts"),
        LeftoverKind::Receipt => ("安装收据", "Receipt"),
        LeftoverKind::HomebrewCask => ("Homebrew cask", "Homebrew cask"),
        LeftoverKind::HomeDotfile => ("家目录里的隐藏文件", "Dotfile in the home folder"),
        LeftoverKind::Other => ("其它", "Other"),
    };
    pick(lang, zh, en)
}

pub fn describe_confidence(lang: Lang, confidence: Confidence) -> &'static str {
    let (zh, en) = match confidence {
        Confidence::Certain => ("确定", "certain"),
        Confidence::Likely => ("很可能", "likely"),
        Confidence::Possible => ("可能", "possible"),
    };
    pick(lang, zh, en)
}

pub fn describe_protect(lang: Lang, reason: ProtectReason) -> &'static str {
    let (zh, en) = match reason {
        ProtectReason::ICloudDrive => (
            "iCloud 云盘和同步到其它设备的文件夹",
            "iCloud Drive and folders synced to your other devices",
        ),
        ProtectReason::Keychain => ("钥匙串", "Keychains"),
        ProtectReason::SshKeys => ("SSH 密钥", "SSH keys"),
        ProtectReason::GpgKeys => ("GPG 密钥", "GPG keys"),
        ProtectReason::Mail => ("邮件", "Mail"),
        ProtectReason::Messages => ("信息", "Messages"),
        ProtectReason::Safari => ("Safari 的数据", "Safari's data"),
        ProtectReason::Contacts => ("通讯录", "Contacts"),
        ProtectReason::Calendars => ("日历", "Calendars"),
        ProtectReason::Notes => ("备忘录", "Notes"),
        ProtectReason::PhotoLibrary => ("照片图库", "A photo library"),
        ProtectReason::MusicLibrary => ("音乐资料库", "A music library"),
        ProtectReason::DeviceBackups => ("iPhone 和 iPad 的备份", "iPhone and iPad backups"),
        ProtectReason::UserFolder => (
            "桌面、文稿、下载这些文件夹本身",
            "The Desktop, Documents, or Downloads folder itself",
        ),
        ProtectReason::RootFolder => (
            "家目录、资源库这些根文件夹本身",
            "The home folder, Library, or another root folder itself",
        ),
        ProtectReason::SystemPath => ("macOS 的系统文件", "macOS's own files"),
        ProtectReason::Wallet => (
            "装了加密钱包扩展的浏览器配置",
            "A browser profile with a crypto wallet extension",
        ),
        ProtectReason::RindItself => ("Rind 自己", "Rind itself"),
        ProtectReason::Unresolvable => (
            "路径对不上盘上的实际位置，按保护处理",
            "The path does not resolve to a real location, so it is treated as protected",
        ),
    };
    pick(lang, zh, en)
}

/// `Size::Unknown` → "未知" / "unknown"，否则 `size::format_bytes`。
pub fn format_size(lang: Lang, size: Size) -> String {
    match size {
        Size::Known(bytes) => rind_core::size::format_bytes(bytes),
        Size::Unknown => pick(lang, "未知", "unknown").to_string(),
    }
}

fn pick(lang: Lang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::ZhCn => zh,
        Lang::En => en,
    }
}

/// 执行时出的错，说人话。`Ledger { partial }` 里已经移走的东西由调用方另外列出来。
pub fn describe_execute_error(lang: Lang, error: &ExecuteError) -> String {
    match (error, lang) {
        (ExecuteError::AppRunning, Lang::ZhCn) => "这个 App 还在运行，先退出它。".to_string(),
        (ExecuteError::AppRunning, Lang::En) => "The app is still running. Quit it first.".to_string(),
        (ExecuteError::NothingSelected, Lang::ZhCn) => "还没有选中任何东西。".to_string(),
        (ExecuteError::NothingSelected, Lang::En) => "Nothing is selected.".to_string(),
        (ExecuteError::NotInPlan(path), Lang::ZhCn) => format!("选中的 {} 不在计划里，先重新扫描。", path.display()),
        (ExecuteError::NotInPlan(path), Lang::En) => format!("{} is not in the plan; scan again first.", path.display()),
        (ExecuteError::AppMoveFailed(why), Lang::ZhCn) => format!("App 本体没能移走（{why}），它的残留也原样留着。"),
        (ExecuteError::AppMoveFailed(why), Lang::En) => format!("The app could not be moved ({why}), so its leftovers were left alone too."),
        (ExecuteError::LedgerRequired, Lang::ZhCn) => "写不了历史记录，所以什么都没动：没有记录的东西放不回来。".to_string(),
        (ExecuteError::LedgerRequired, Lang::En) => "History is unavailable, so nothing was moved: what is not recorded cannot be put back.".to_string(),
        (ExecuteError::Ledger { error, partial }, Lang::ZhCn) => format!(
            "历史记录写不进去（{error}），已停止。此前移到废纸篓 {} 项、失败 {} 项。",
            partial.moved.len(),
            partial.failed.len()
        ),
        (ExecuteError::Ledger { error, partial }, Lang::En) => format!(
            "History could not be written ({error}); stopped. Before that, {} items moved to the Trash and {} failed.",
            partial.moved.len(),
            partial.failed.len()
        ),
    }
}

/// key 不在表里：把它本身返回。`&'static str` 要求我们把它留住；漏翻是程序错误，但同一个 key 只留一份，
/// 渲染循环里用错了 key 也不会越漏越多。
fn missing(key: &str) -> &'static str {
    static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = seen.get(key) {
        return existing;
    }
    let leaked: &'static str = Box::leak(key.to_string().into_boxed_str());
    seen.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;
    use rind_core::AppRef;

    #[test]
    fn language_detection() {
        assert_eq!(
            Lang::from_vars(Some("en"), Some("zh_CN.UTF-8"), None),
            Lang::En,
            "RIND_LANG wins"
        );
        assert_eq!(
            Lang::from_vars(Some("zh"), Some("en_US.UTF-8"), None),
            Lang::ZhCn
        );
        assert_eq!(
            Lang::from_vars(Some("fr"), None, Some("en_US.UTF-8")),
            Lang::En,
            "unknown RIND_LANG falls through"
        );
        assert_eq!(
            Lang::from_vars(None, Some("zh_CN.UTF-8"), Some("en_US.UTF-8")),
            Lang::ZhCn,
            "LC_ALL before LANG"
        );
        assert_eq!(Lang::from_vars(None, None, Some("zh_TW.UTF-8")), Lang::ZhCn);
        assert_eq!(Lang::from_vars(None, None, Some("en_US.UTF-8")), Lang::En);
        assert_eq!(Lang::from_vars(None, None, Some("C")), Lang::En);
        assert_eq!(
            Lang::from_vars(None, None, None),
            Lang::ZhCn,
            "nothing set means Chinese"
        );
        assert_eq!(Lang::ZhCn.tag(), "zh-CN");
    }

    #[test]
    fn table_lookups() {
        assert_eq!(t(Lang::ZhCn, "nav.apps"), "应用");
        assert_eq!(t(Lang::En, "nav.apps"), "Apps");
        assert_eq!(t(Lang::ZhCn, "no.such.key"), "no.such.key");
        assert!(t(Lang::ZhCn, "summary.selected").contains("{n}"));
        assert!(t(Lang::En, "summary.selected").contains("{size}"));
        for key in [
            "app.title",
            "nav.apps",
            "nav.history",
            "nav.settings",
            "action.scan",
            "action.trash",
            "action.restore",
            "action.quit_app",
            "action.force_quit",
            "action.select_suggested",
            "action.clear",
            "state.scanning",
            "state.measuring",
            "state.running",
            "state.nothing_selected",
            "state.dry_run",
            "col.path",
            "col.kind",
            "col.size",
            "col.confidence",
            "col.reason",
            "summary.selected",
            "summary.freed",
            "confirm.title",
            "confirm.body",
            "history.empty",
            "history.restored",
        ] {
            assert_ne!(t(Lang::ZhCn, key), key, "{key} has a Chinese string");
            assert_ne!(t(Lang::En, key), key, "{key} has an English string");
        }
    }

    #[test]
    fn every_key_is_unique_and_filled_in_both_languages() {
        let keys: Vec<&str> = keys().collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate keys");
        for (key, zh, en) in strings::TABLE {
            assert!(
                !zh.is_empty() && !en.is_empty(),
                "{key} is missing a translation"
            );
        }
    }

    #[test]
    fn fills_placeholders() {
        assert_eq!(
            fill("选了 {n} 项，共 {size}", &[("n", "3"), ("size", "1.2 GB")]),
            "选了 3 项，共 1.2 GB"
        );
        assert_eq!(fill("no holes", &[("n", "3")]), "no holes");
    }

    #[test]
    fn evidence_in_both_languages() {
        let ev = |kind, confidence| Evidence {
            kind,
            confidence,
            matched: "com.foo.bar".to_string(),
        };
        assert_eq!(
            describe_evidence(Lang::ZhCn, &ev(EvidenceKind::BundleId, Confidence::Certain)),
            "名字里含它的 bundle id：com.foo.bar"
        );
        assert_eq!(
            describe_evidence(Lang::En, &ev(EvidenceKind::BundleId, Confidence::Certain)),
            "Its name holds the app's bundle id: com.foo.bar"
        );
        assert!(
            describe_evidence(Lang::ZhCn, &ev(EvidenceKind::Name, Confidence::Likely))
                .contains("相同")
        );
        assert!(
            describe_evidence(Lang::ZhCn, &ev(EvidenceKind::Name, Confidence::Possible))
                .contains("含")
        );
        assert!(describe_evidence(
            Lang::En,
            &ev(
                EvidenceKind::CaskZap {
                    token: "obsidian".into()
                },
                Confidence::Likely
            )
        )
        .contains("obsidian"));
        assert_eq!(
            describe_evidence(
                Lang::ZhCn,
                &Evidence {
                    kind: EvidenceKind::Rule {
                        rule: "微信".into()
                    },
                    confidence: Confidence::Certain,
                    matched: String::new()
                }
            ),
            "规则库「微信」点名了它"
        );

        // 每个变体都有话说，而且中英不一样。
        let kinds = [
            EvidenceKind::BundleId,
            EvidenceKind::EmbeddedBundleId,
            EvidenceKind::AppGroup,
            EvidenceKind::BundleIdPrefix,
            EvidenceKind::TeamId,
            EvidenceKind::Name,
            EvidenceKind::LaunchdProgram,
            EvidenceKind::CaskZap { token: "x".into() },
            EvidenceKind::Receipt,
            EvidenceKind::Rule { rule: "r".into() },
        ];
        for kind in kinds {
            let e = ev(kind, Confidence::Likely);
            let zh = describe_evidence(Lang::ZhCn, &e);
            let en = describe_evidence(Lang::En, &e);
            assert!(!zh.is_empty() && !en.is_empty() && zh != en, "{:?}", e.kind);
        }
    }

    #[test]
    fn hold_backs_in_both_languages() {
        let foo = AppRef {
            name: "Foo".into(),
            bundle_id: None,
            path: "/Applications/Foo.app".into(),
        };
        let bar = AppRef {
            name: "Bar".into(),
            bundle_id: None,
            path: "/Applications/Bar.app".into(),
        };
        let shared = HoldBack::SharedWith {
            apps: vec![foo.clone(), bar],
        };
        assert_eq!(
            describe_hold_back(Lang::ZhCn, &shared),
            "另外 2 个 App 也在用：Foo、Bar"
        );
        assert_eq!(
            describe_hold_back(Lang::En, &shared),
            "2 other apps use it too: Foo, Bar"
        );
        assert_eq!(
            describe_hold_back(Lang::En, &HoldBack::SharedWith { apps: vec![foo] }),
            "Another app uses it too: Foo"
        );
        assert_eq!(
            describe_hold_back(
                Lang::ZhCn,
                &HoldBack::Protected {
                    why: ProtectReason::Keychain
                }
            ),
            "受保护：钥匙串"
        );
        assert_eq!(
            describe_hold_back(
                Lang::En,
                &HoldBack::Protected {
                    why: ProtectReason::Keychain
                }
            ),
            "Protected: Keychains"
        );
        for hb in [
            HoldBack::Unreadable,
            HoldBack::NeedsAdmin,
            HoldBack::SystemOwned,
        ] {
            assert_ne!(
                describe_hold_back(Lang::ZhCn, &hb),
                describe_hold_back(Lang::En, &hb)
            );
        }
    }

    #[test]
    fn every_enum_variant_is_named() {
        let kinds = [
            LeftoverKind::AppSupport,
            LeftoverKind::Cache,
            LeftoverKind::Preferences,
            LeftoverKind::Container,
            LeftoverKind::GroupContainer,
            LeftoverKind::LaunchAgent,
            LeftoverKind::LaunchDaemon,
            LeftoverKind::SavedState,
            LeftoverKind::Log,
            LeftoverKind::WebData,
            LeftoverKind::Cookies,
            LeftoverKind::ApplicationScripts,
            LeftoverKind::Receipt,
            LeftoverKind::HomebrewCask,
            LeftoverKind::HomeDotfile,
            LeftoverKind::Other,
        ];
        for kind in kinds {
            assert!(!describe_kind(Lang::ZhCn, kind).is_empty());
            assert!(!describe_kind(Lang::En, kind).is_empty());
        }
        for c in [
            Confidence::Certain,
            Confidence::Likely,
            Confidence::Possible,
        ] {
            assert_ne!(
                describe_confidence(Lang::ZhCn, c),
                describe_confidence(Lang::En, c)
            );
        }
        let reasons = [
            ProtectReason::ICloudDrive,
            ProtectReason::Keychain,
            ProtectReason::SshKeys,
            ProtectReason::GpgKeys,
            ProtectReason::Mail,
            ProtectReason::Messages,
            ProtectReason::Safari,
            ProtectReason::Contacts,
            ProtectReason::Calendars,
            ProtectReason::Notes,
            ProtectReason::PhotoLibrary,
            ProtectReason::MusicLibrary,
            ProtectReason::DeviceBackups,
            ProtectReason::UserFolder,
            ProtectReason::RootFolder,
            ProtectReason::SystemPath,
            ProtectReason::Wallet,
            ProtectReason::RindItself,
            ProtectReason::Unresolvable,
        ];
        for r in reasons {
            assert_ne!(
                describe_protect(Lang::ZhCn, r),
                describe_protect(Lang::En, r),
                "{r:?}"
            );
        }
    }

    #[test]
    fn unknown_size_is_said_not_zeroed() {
        assert_eq!(format_size(Lang::ZhCn, Size::Unknown), "未知");
        assert_eq!(format_size(Lang::En, Size::Unknown), "unknown");
    }

    #[test]
    fn a_missing_key_is_leaked_once_only() {
        let a = t(Lang::ZhCn, "missing.key.for.test");
        let b = t(Lang::En, "missing.key.for.test");
        assert_eq!(a, "missing.key.for.test");
        assert!(std::ptr::eq(a, b), "the same leaked string is handed back");
    }

    #[test]
    fn execute_errors_in_both_languages() {
        let partial = rind_core::RemovalOutcome {
            moved: vec![("/a".into(), "/t/a".into())],
            failed: vec![("/b".into(), "x".into())],
            ..Default::default()
        };
        let errors = [
            ExecuteError::AppRunning,
            ExecuteError::NothingSelected,
            ExecuteError::NotInPlan("/x".into()),
            ExecuteError::AppMoveFailed("gone".into()),
            ExecuteError::LedgerRequired,
            ExecuteError::Ledger {
                error: "disk full".into(),
                partial: Box::new(partial),
            },
        ];
        for e in &errors {
            let zh = describe_execute_error(Lang::ZhCn, e);
            let en = describe_execute_error(Lang::En, e);
            assert!(!zh.is_empty() && !en.is_empty() && zh != en, "{e}");
        }
        assert!(describe_execute_error(Lang::ZhCn, &errors[5]).contains("1 项、失败 1 项"));
        assert!(describe_execute_error(Lang::En, &errors[5]).contains("1 items moved"));
        assert_eq!(
            describe_hold_back(
                Lang::ZhCn,
                &HoldBack::Protected {
                    why: ProtectReason::Unresolvable
                }
            ),
            "受保护：路径对不上盘上的实际位置，按保护处理"
        );
    }
}
