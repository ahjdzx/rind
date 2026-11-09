//! `RIND_DEMO=1` 时的示例后端：一组中文 App、几组孤儿、各类缓存、一棵空间树、若干启动项和历史。
//! 只用来跑通和目测界面——**绝不读写真实系统**：移动、停用、退出都只改 `DemoState` 里的内存状态。
//! 文案一律走 `rind-i18n`，和真实模式一字不差。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rind_core::cleanup::{CleanupCategory, CleanupItem, CleanupReport};
use rind_core::disk::DiskUsage;
use rind_core::launch::{LaunchDomain, LaunchItem, LaunchStatus};
use rind_core::orphans::{OrphanGroup, OrphanReport};
use rind_core::remove::{Batch, Removable};
use rind_core::space::{LargeFile, NodeKind, SpaceNode, SpaceReport};
use rind_core::{
    AppRef, AppSource, Confidence, Env, Evidence, EvidenceKind, HistoryItem, HistoryRecord,
    HoldBack, InstalledApp, LeftoverKind, LeftoverMatch, OrphanSignal, ProtectReason, RecordKind,
    RemovalOutcome, RestoreOutcome, ScanProgress, Selection, Size, Source, UninstallPlan,
};
use rind_i18n::Lang;

use crate::model;
use crate::services::{plan_view as view_of_plan, CleanupPrefs, PlanView};

const MB: u64 = 1_000_000;
const KB: u64 = 1_000;

const WECHAT: &str = "com.tencent.xinWeChat";

fn days_ago(days: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::days(days)
}

/// 停一会儿，假装在干活；被取消就返回 false。
fn pause(cancel: &AtomicBool, ms: u64) -> bool {
    std::thread::sleep(Duration::from_millis(ms));
    !cancel.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------------------------
// App

fn app(
    name: &str,
    bundle_id: &str,
    version: &str,
    team: &str,
    source: AppSource,
    size: u64,
) -> InstalledApp {
    InstalledApp {
        path: PathBuf::from(format!("/Applications/{name}.app")),
        name: name.to_string(),
        bundle_id: Some(bundle_id.to_string()),
        version: Some(version.to_string()),
        executable: Some(PathBuf::from(format!(
            "/Applications/{name}.app/Contents/MacOS/{name}"
        ))),
        team_id: Some(team.to_string()),
        embedded_ids: vec![format!("{bundle_id}.helper")],
        app_groups: vec![format!("group.{bundle_id}")],
        source,
        size: Size::Known(size),
        is_system: false,
    }
}

/// 示例里"装着"的 App，含一个 Apple 自带的（列表里不显示）。
pub fn apps() -> Vec<InstalledApp> {
    use AppSource::*;
    let mut apps = vec![
        app("微信", WECHAT, "4.0.3", "5A4RE8SF68", AppStore, 735 * MB),
        app(
            "钉钉",
            "com.alibaba.DingTalkMac",
            "7.6.15",
            "4FT2CR3L2X",
            Other,
            1_210 * MB,
        ),
        app(
            "飞书",
            "com.electron.lark",
            "7.32.8",
            "3D6GJ8CYLB",
            Other,
            982 * MB,
        ),
        app(
            "网易云音乐",
            "com.netease.163music",
            "3.0.18",
            "9Z6LH8WE7Q",
            AppStore,
            310 * MB,
        ),
        app(
            "百度网盘",
            "com.baidu.BaiduNetdisk-mac",
            "4.38.1",
            "4Z2RWGQ9HV",
            Other,
            423 * MB,
        ),
        app(
            "腾讯会议",
            "com.tencent.meeting",
            "3.28.2",
            "88L2Q4487U",
            AppStore,
            561 * MB,
        ),
        app(
            "Obsidian",
            "md.obsidian",
            "1.7.4",
            "6JSW4SJWN9",
            Homebrew("obsidian".into()),
            412 * MB,
        ),
        app(
            "Visual Studio Code",
            "com.microsoft.VSCode",
            "1.95.2",
            "UBF8T346G9",
            Homebrew("visual-studio-code".into()),
            598 * MB,
        ),
        app(
            "Docker",
            "com.docker.docker",
            "4.35.1",
            "9BNSXJN65R",
            Other,
            1_980 * MB,
        ),
        app(
            "Figma",
            "com.figma.Desktop",
            "124.6.5",
            "T8RA8NE3B7",
            Other,
            289 * MB,
        ),
    ];
    let mut mail = app(
        "Mail",
        "com.apple.mail",
        "16.0",
        "APPLE",
        AppSource::Other,
        30 * MB,
    );
    mail.path = PathBuf::from("/System/Applications/Mail.app");
    mail.is_system = true;
    apps.push(mail);
    apps
}

pub fn disk() -> DiskUsage {
    DiskUsage {
        total: 494_384_795_648,
        available: 71_234_000_000,
    }
}

pub fn project_roots(env: &Env) -> Vec<PathBuf> {
    vec![env.home.join("work"), env.home.join("Projects")]
}

// ---------------------------------------------------------------------------------------------
// 卸载计划

fn evidence(kind: EvidenceKind, confidence: Confidence, matched: &str) -> Evidence {
    Evidence {
        kind,
        confidence,
        matched: matched.to_string(),
    }
}

fn leftover(
    env: &Env,
    path: &str,
    kind: LeftoverKind,
    size: Size,
    evidence: Evidence,
    rivals: Vec<AppRef>,
    hold_back: Option<HoldBack>,
) -> LeftoverMatch {
    LeftoverMatch {
        path: env.expand(path),
        kind,
        size,
        evidence,
        rivals,
        hold_back,
    }
}

/// 一份把每种把握、阻拦、未知大小都摆出来的计划。
fn leftovers(env: &Env, app: &InstalledApp) -> Vec<LeftoverMatch> {
    let id = app.bundle_id.clone().unwrap_or_default();
    let name = app.name.clone();
    let lower = name.to_lowercase().replace(' ', "-");
    let team = app.team_id.clone().unwrap_or_default();
    let rival = AppRef {
        name: format!("{name} Beta"),
        bundle_id: Some(format!("{id}.beta")),
        path: PathBuf::from(format!("/Applications/{name} Beta.app")),
    };
    let mut rows = vec![
        leftover(
            env,
            &format!("~/Library/Application Support/{name}"),
            LeftoverKind::AppSupport,
            Size::Known(861 * MB),
            evidence(EvidenceKind::Name, Confidence::Likely, &name),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Caches/{id}"),
            LeftoverKind::Cache,
            Size::Known(1_420 * MB),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Preferences/{id}.plist"),
            LeftoverKind::Preferences,
            Size::Known(24 * KB),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Saved Application State/{id}.savedState"),
            LeftoverKind::SavedState,
            Size::Known(118 * KB),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Logs/{name}"),
            LeftoverKind::Log,
            Size::Known(5_300 * KB),
            evidence(EvidenceKind::Name, Confidence::Likely, &name),
            vec![rival.clone()],
            Some(HoldBack::SharedWith {
                apps: vec![rival.clone()],
            }),
        ),
        leftover(
            env,
            &format!("~/Library/HTTPStorages/{id}"),
            LeftoverKind::WebData,
            Size::Known(2_100 * KB),
            evidence(EvidenceKind::BundleIdPrefix, Confidence::Likely, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Containers/{id}"),
            LeftoverKind::Container,
            Size::Unknown,
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Group Containers/{team}.{id}"),
            LeftoverKind::GroupContainer,
            Size::Unknown,
            evidence(EvidenceKind::TeamId, Confidence::Likely, &team),
            vec![],
            Some(HoldBack::Unreadable),
        ),
        leftover(
            env,
            &format!("~/Library/Mobile Documents/iCloud~{}", id.replace('.', "~")),
            LeftoverKind::Other,
            Size::Known(2_300 * MB),
            evidence(
                EvidenceKind::AppGroup,
                Confidence::Certain,
                &format!("group.{id}"),
            ),
            vec![],
            Some(HoldBack::Protected {
                why: ProtectReason::ICloudDrive,
            }),
        ),
        leftover(
            env,
            &format!("/Library/LaunchDaemons/{id}.helper.plist"),
            LeftoverKind::LaunchDaemon,
            Size::Known(2 * KB),
            evidence(
                EvidenceKind::LaunchdProgram,
                Confidence::Certain,
                &format!("{name}.app/Contents/MacOS/helper"),
            ),
            vec![],
            Some(HoldBack::NeedsAdmin),
        ),
        leftover(
            env,
            "~/Library/Application Support/Knowledge",
            LeftoverKind::AppSupport,
            Size::Known(90 * MB),
            evidence(EvidenceKind::Name, Confidence::Possible, "Knowledge"),
            vec![],
            Some(HoldBack::SystemOwned),
        ),
    ];
    if let AppSource::Homebrew(token) = &app.source {
        rows.push(leftover(
            env,
            &format!("~/Library/Caches/Homebrew/downloads/a1b2c3--{lower}.dmg"),
            LeftoverKind::HomebrewCask,
            Size::Known(183 * MB),
            evidence(
                EvidenceKind::CaskZap {
                    token: token.clone(),
                },
                Confidence::Likely,
                &lower,
            ),
            vec![],
            None,
        ));
    }
    if matches!(app.source, AppSource::Other) {
        rows.push(leftover(
            env,
            &format!("/private/var/db/receipts/{id}.pkg.bom"),
            LeftoverKind::Receipt,
            Size::Known(41 * KB),
            evidence(EvidenceKind::Receipt, Confidence::Certain, &id),
            vec![],
            None,
        ));
    }
    if id == "com.microsoft.VSCode" {
        rows.push(leftover(
            env,
            "~/.vscode",
            LeftoverKind::HomeDotfile,
            Size::Known(640 * MB),
            evidence(
                EvidenceKind::Rule { rule: id.clone() },
                Confidence::Certain,
                "~/.vscode",
            ),
            vec![],
            None,
        ));
    }
    rows
}

/// 模拟一次扫描：报几次进度，停一会儿，再给结果。
pub fn plan_view(
    env: &Env,
    app: &InstalledApp,
    lang: Lang,
    cancel: &AtomicBool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> PlanView {
    for location in [
        "~/Library/Application Support",
        "~/Library/Caches",
        "~/Library/Containers",
        "/Library",
    ] {
        on_progress(ScanProgress::Location(location.to_string()));
        if !pause(cancel, 180) {
            break;
        }
    }
    let plan = UninstallPlan {
        app: app.clone(),
        app_hold_back: None,
        leftovers: leftovers(env, app),
        running: app.bundle_id.as_deref() == Some(WECHAT),
    };
    let total = plan.leftovers.len();
    for done in 1..=total {
        on_progress(ScanProgress::Measuring { done, total });
        if !pause(cancel, 40) {
            break;
        }
    }
    on_progress(ScanProgress::Finished);
    view_of_plan(env, Arc::new(plan), lang)
}

// ---------------------------------------------------------------------------------------------
// 孤儿

#[allow(clippy::too_many_arguments)]
fn orphan_item(
    env: &Env,
    path: &str,
    kind: LeftoverKind,
    size: Size,
    signal: OrphanSignal,
    confidence: Confidence,
    matched: &str,
    hold_back: Option<HoldBack>,
) -> LeftoverMatch {
    let rivals = match &hold_back {
        Some(HoldBack::SharedWith { apps }) => apps.clone(),
        _ => Vec::new(),
    };
    leftover(
        env,
        path,
        kind,
        size,
        evidence(EvidenceKind::Orphan { signal }, confidence, matched),
        rivals,
        hold_back,
    )
}

fn orphan_groups(env: &Env) -> Vec<OrphanGroup> {
    use Confidence::*;
    use LeftoverKind::*;
    use OrphanSignal::*;
    let wechat = AppRef {
        name: "微信".into(),
        bundle_id: Some(WECHAT.into()),
        path: PathBuf::from("/Applications/微信.app"),
    };
    let group = |key: &str, name: &str, items: Vec<LeftoverMatch>| OrphanGroup {
        key: key.into(),
        display_name: name.into(),
        items,
    };
    vec![
        group(
            "com.xunlei.Thunder",
            "迅雷",
            vec![
                orphan_item(
                    env,
                    "~/Library/Application Support/Thunder",
                    AppSupport,
                    Size::Known(1_320 * MB),
                    KnownAppGone,
                    Likely,
                    "Thunder",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Caches/com.xunlei.Thunder",
                    Cache,
                    Size::Known(680 * MB),
                    BundleIdGone,
                    Likely,
                    "com.xunlei.Thunder",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Preferences/com.xunlei.Thunder.plist",
                    Preferences,
                    Size::Known(36 * KB),
                    BundleIdGone,
                    Likely,
                    "com.xunlei.Thunder",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Saved Application State/com.xunlei.Thunder.savedState",
                    SavedState,
                    Size::Known(210 * KB),
                    BundleIdGone,
                    Likely,
                    "com.xunlei.Thunder",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/LaunchAgents/com.xunlei.Thunder.helper.plist",
                    LaunchAgent,
                    Size::Known(2 * KB),
                    ProgramMissing,
                    Certain,
                    "/Applications/Thunder.app/Contents/MacOS/helper",
                    None,
                ),
            ],
        ),
        group(
            "com.tencent.QQMusicMac",
            "QQ音乐",
            vec![
                orphan_item(
                    env,
                    "~/Library/Containers/com.tencent.QQMusicMac",
                    Container,
                    Size::Known(2_410 * MB),
                    BundleIdGone,
                    Likely,
                    "com.tencent.QQMusicMac",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Group Containers/5A4RE8SF68.com.tencent",
                    GroupContainer,
                    Size::Known(96 * MB),
                    BundleIdGone,
                    Likely,
                    "5A4RE8SF68.com.tencent",
                    Some(HoldBack::SharedWith { apps: vec![wechat] }),
                ),
            ],
        ),
        group(
            "com.bohemiancoding.sketch3",
            "Sketch",
            vec![
                orphan_item(
                    env,
                    "~/Library/Application Support/com.bohemiancoding.sketch3",
                    AppSupport,
                    Size::Known(1_100 * MB),
                    BundleIdGone,
                    Likely,
                    "com.bohemiancoding.sketch3",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Caches/com.bohemiancoding.sketch3",
                    Cache,
                    Size::Known(420 * MB),
                    BundleIdGone,
                    Likely,
                    "com.bohemiancoding.sketch3",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/HTTPStorages/com.bohemiancoding.sketch3",
                    WebData,
                    Size::Known(3_200 * KB),
                    BundleIdGone,
                    Likely,
                    "com.bohemiancoding.sketch3",
                    None,
                ),
            ],
        ),
        group(
            "us.zoom.xos",
            "Zoom",
            vec![
                orphan_item(
                    env,
                    "~/Library/Application Support/zoom.us",
                    AppSupport,
                    Size::Known(232 * MB),
                    KnownAppGone,
                    Likely,
                    "zoom.us",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Logs/zoom.us",
                    Log,
                    Size::Known(85 * MB),
                    KnownAppGone,
                    Likely,
                    "zoom.us",
                    None,
                ),
                orphan_item(
                    env,
                    "~/Library/Group Containers/BJ4HAAB9B3.ZoomClient3rd",
                    GroupContainer,
                    Size::Unknown,
                    KnownAppGone,
                    Likely,
                    "ZoomClient3rd",
                    Some(HoldBack::Unreadable),
                ),
                orphan_item(
                    env,
                    "/Library/Caches/us.zoom.xos",
                    Cache,
                    Size::Known(12 * MB),
                    BundleIdGone,
                    Likely,
                    "us.zoom.xos",
                    Some(HoldBack::NeedsAdmin),
                ),
            ],
        ),
        group(
            "com.parallels.desktop.console",
            "Parallels Desktop",
            vec![
                orphan_item(
                    env,
                    "/Library/LaunchDaemons/com.parallels.desktop.launchdaemon.plist",
                    LaunchDaemon,
                    Size::Known(3 * KB),
                    ProgramMissing,
                    Certain,
                    "/Applications/Parallels Desktop.app/Contents/MacOS/prl_launch_daemon",
                    Some(HoldBack::NeedsAdmin),
                ),
                orphan_item(
                    env,
                    "~/Library/Preferences/com.parallels.desktop.console.plist",
                    Preferences,
                    Size::Known(14 * KB),
                    BundleIdGone,
                    Likely,
                    "com.parallels.desktop.console",
                    None,
                ),
            ],
        ),
        group(
            "kugoumusic",
            "KugouMusic",
            vec![orphan_item(
                env,
                "~/Library/Application Support/KugouMusic",
                AppSupport,
                Size::Known(156 * MB),
                NameGone,
                Possible,
                "KugouMusic",
                None,
            )],
        ),
    ]
}

pub fn orphans(
    env: &Env,
    gone: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> OrphanReport {
    for location in [
        "~/Library/Application Support",
        "~/Library/Caches",
        "~/Library/Containers",
        "~/Library/Group Containers",
        "~/Library/Preferences",
        "~/Library/LaunchAgents",
        "/Library/LaunchDaemons",
    ] {
        on_progress(ScanProgress::Location(location.to_string()));
        if !pause(cancel, 220) {
            return OrphanReport::default();
        }
    }
    let mut groups = orphan_groups(env);
    for group in &mut groups {
        group.items.retain(|m| !model::is_gone(&m.path, gone));
    }
    groups.retain(|g| !g.items.is_empty());
    let total: usize = groups.iter().map(|g| g.items.len()).sum();
    for done in 1..=total {
        on_progress(ScanProgress::Measuring { done, total });
        if !pause(cancel, 60) {
            return OrphanReport::default();
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.known_size()));
    on_progress(ScanProgress::Finished);
    OrphanReport {
        groups,
        degraded: false,
    }
}

// ---------------------------------------------------------------------------------------------
// 缓存清理

fn cleanup_item(
    path: PathBuf,
    category: CleanupCategory,
    label: &str,
    size: Size,
    days: i64,
    hold_back: Option<HoldBack>,
    suggested: bool,
) -> CleanupItem {
    let suggested = suggested && hold_back.is_none() && matches!(size, Size::Known(_));
    CleanupItem {
        path,
        category,
        label: label.to_string(),
        size,
        modified: Some(days_ago(days)),
        hold_back,
        suggested,
    }
}

pub fn cleanup(
    env: &Env,
    wechat_running: bool,
    gone: &HashSet<PathBuf>,
    prefs: &CleanupPrefs,
    cancel: &AtomicBool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> CleanupReport {
    use CleanupCategory::*;
    let home = |p: &str| env.expand(p);
    let in_use = |app: &str| Some(HoldBack::InUse { app: app.into() });
    let roots = if prefs.roots.is_empty() {
        project_roots(env)
    } else {
        prefs.roots.clone()
    };
    let root_a = roots[0].clone();
    let root_b = roots.get(1).cloned().unwrap_or_else(|| root_a.clone());
    let old_enough = |days: i64| days >= i64::from(prefs.min_age_days);

    let mut items = vec![
        cleanup_item(
            home("~/Library/Caches/com.spotify.client"),
            AppCaches,
            "Spotify",
            Size::Known(1_820 * MB),
            0,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.tencent.xinWeChat"),
            AppCaches,
            "微信",
            Size::Known(1_260 * MB),
            0,
            if wechat_running {
                in_use("微信")
            } else {
                None
            },
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.electron.lark"),
            AppCaches,
            "飞书",
            Size::Known(921 * MB),
            1,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.netease.163music"),
            AppCaches,
            "网易云音乐",
            Size::Known(640 * MB),
            2,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.figma.Desktop"),
            AppCaches,
            "Figma",
            Size::Known(312 * MB),
            3,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.adobe.acc.AdobeCreativeCloud"),
            AppCaches,
            "Adobe Creative Cloud",
            Size::Known(205 * MB),
            12,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/com.alibaba.DingTalkMac"),
            AppCaches,
            "钉钉",
            Size::Unknown,
            1,
            Some(HoldBack::Unreadable),
            false,
        ),
        cleanup_item(
            home("~/Library/Logs/DiagnosticReports"),
            Logs,
            "诊断报告",
            Size::Known(84 * MB),
            1,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Logs/Docker Desktop"),
            Logs,
            "Docker",
            Size::Known(236 * MB),
            0,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Logs/JetBrains"),
            Logs,
            "JetBrains",
            Size::Known(46 * MB),
            6,
            None,
            true,
        ),
        cleanup_item(
            PathBuf::from("/Library/Logs/DiagnosticReports"),
            Logs,
            "系统诊断报告",
            Size::Known(120 * MB),
            2,
            Some(HoldBack::NeedsAdmin),
            false,
        ),
        cleanup_item(
            home("~/Library/Caches/Google/Chrome"),
            BrowserCaches,
            "Google Chrome",
            Size::Known(2_140 * MB),
            0,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/company.thebrowser.Browser"),
            BrowserCaches,
            "Arc",
            Size::Known(1_100 * MB),
            0,
            in_use("Arc"),
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/Microsoft Edge"),
            BrowserCaches,
            "Microsoft Edge",
            Size::Known(762 * MB),
            9,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Developer/Xcode/DerivedData"),
            Xcode,
            "DerivedData",
            Size::Known(14_200 * MB),
            4,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Developer/CoreSimulator/Caches"),
            Xcode,
            "模拟器缓存",
            Size::Known(3_100 * MB),
            8,
            None,
            true,
        ),
        cleanup_item(
            home("~/.gradle/caches"),
            PackageCaches,
            "Gradle",
            Size::Known(4_100 * MB),
            20,
            None,
            true,
        ),
        cleanup_item(
            home("~/.cargo/registry"),
            PackageCaches,
            "Cargo",
            Size::Known(3_300 * MB),
            1,
            None,
            true,
        ),
        cleanup_item(
            home("~/.npm/_cacache"),
            PackageCaches,
            "npm",
            Size::Known(2_400 * MB),
            3,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/Homebrew"),
            PackageCaches,
            "Homebrew",
            Size::Known(1_600 * MB),
            6,
            None,
            true,
        ),
        cleanup_item(
            home("~/go/pkg/mod"),
            PackageCaches,
            "Go modules",
            Size::Known(1_200 * MB),
            14,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/Yarn"),
            PackageCaches,
            "Yarn",
            Size::Known(940 * MB),
            30,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Caches/pip"),
            PackageCaches,
            "pip",
            Size::Known(860 * MB),
            45,
            None,
            true,
        ),
        cleanup_item(
            home("~/Library/Developer/CoreSimulator/Devices/5F2C1B7A-iPhone-14-iOS-16-4"),
            NeedsReview,
            "iPhone 14 · iOS 16.4",
            Size::Known(6_200 * MB),
            260,
            None,
            false,
        ),
        cleanup_item(
            home("~/Library/Developer/Xcode/Archives"),
            NeedsReview,
            "Xcode Archives",
            Size::Known(5_600 * MB),
            75,
            None,
            false,
        ),
        cleanup_item(
            home("~/Library/Developer/Xcode/iOS DeviceSupport/17.5 (21F79)"),
            NeedsReview,
            "iOS DeviceSupport 17.5",
            Size::Known(4_800 * MB),
            160,
            None,
            false,
        ),
    ];
    for (root, project, artifact, size, days) in [
        (&root_a, "rind", "target", 6_800 * MB, 2),
        (&root_a, "ml-demo", ".venv", 2_200 * MB, 95),
        (&root_a, "old-blog", "node_modules", 780 * MB, 120),
        (&root_b, "website", "node_modules", 1_100 * MB, 10),
        (&root_b, "android-demo", "build", 640 * MB, 210),
    ] {
        items.push(cleanup_item(
            root.join(project).join(artifact),
            DevArtifacts,
            &format!("{project} / {artifact}"),
            Size::Known(size),
            days,
            None,
            old_enough(days),
        ));
    }
    items.retain(|i| !model::is_gone(&i.path, gone));

    for location in [
        "~/Library/Caches",
        "~/Library/Logs",
        "~/Library/Developer",
        "~/.npm",
        "~/.cargo",
        "~/.gradle",
    ] {
        on_progress(ScanProgress::Location(location.to_string()));
        if !pause(cancel, 200) {
            return CleanupReport::default();
        }
    }
    let total = items.len();
    for done in (1..=total).step_by(3) {
        on_progress(ScanProgress::Measuring { done, total });
        if !pause(cancel, 50) {
            return CleanupReport::default();
        }
    }
    on_progress(ScanProgress::Finished);
    CleanupReport { items }
}

// ---------------------------------------------------------------------------------------------
// 空间分析

/// 示例树的一个节点：文件，或一个"里面有很多小文件"的目录（`blob`），或普通目录。
enum T {
    File {
        name: String,
        bytes: u64,
        modified: i64,
        accessed: i64,
    },
    Blob {
        name: String,
        bytes: u64,
        files: u64,
        modified: i64,
    },
    Dir {
        name: String,
        children: Vec<T>,
    },
}

fn f(name: &str, bytes: u64, modified: i64, accessed: i64) -> T {
    T::File {
        name: name.to_string(),
        bytes,
        modified,
        accessed,
    }
}

fn b(name: &str, bytes: u64, files: u64, modified: i64) -> T {
    T::Blob {
        name: name.to_string(),
        bytes,
        files,
        modified,
    }
}

fn d(name: &str, children: Vec<T>) -> T {
    T::Dir {
        name: name.to_string(),
        children,
    }
}

fn home_tree() -> Vec<T> {
    vec![
        d(
            "Library",
            vec![
                d(
                    "Application Support",
                    vec![
                        d(
                            "Google",
                            vec![d(
                                "Chrome",
                                vec![
                                    d(
                                        "Default",
                                        vec![
                                            b("Cache_Data", 1_200 * MB, 3_400, 0),
                                            b("IndexedDB", 640 * MB, 900, 1),
                                            f("History", 48 * MB, 0, 0),
                                        ],
                                    ),
                                    b("Profile 1", 380 * MB, 800, 20),
                                ],
                            )],
                        ),
                        d(
                            "Code",
                            vec![
                                d(
                                    "User",
                                    vec![
                                        b("workspaceStorage", 1_100 * MB, 2_100, 0),
                                        b("globalStorage", 220 * MB, 400, 2),
                                    ],
                                ),
                                b("CachedExtensionVSIXs", 340 * MB, 60, 40),
                            ],
                        ),
                        b("Thunder", 1_320 * MB, 1_800, 200),
                        b("com.bohemiancoding.sketch3", 1_100 * MB, 640, 300),
                        d("MobileSync", vec![b("Backup", 18_400 * MB, 50_000, 90)]),
                    ],
                ),
                d(
                    "Caches",
                    vec![
                        b("com.spotify.client", 1_820 * MB, 2_400, 0),
                        d("Google", vec![b("Chrome", 2_140 * MB, 5_200, 0)]),
                        b("com.tencent.xinWeChat", 1_260 * MB, 3_000, 0),
                        b("Homebrew", 1_600 * MB, 140, 6),
                        b("com.xunlei.Thunder", 680 * MB, 900, 200),
                    ],
                ),
                d(
                    "Containers",
                    vec![
                        d(
                            "com.tencent.xinWeChat",
                            vec![d(
                                "Data",
                                vec![d(
                                    "Library",
                                    vec![
                                        b("Application Support", 9_400 * MB, 26_000, 0),
                                        b("Caches", 640 * MB, 2_000, 0),
                                    ],
                                )],
                            )],
                        ),
                        b("com.tencent.QQMusicMac", 2_410 * MB, 1_200, 180),
                    ],
                ),
                d(
                    "Developer",
                    vec![
                        d(
                            "Xcode",
                            vec![
                                b("DerivedData", 14_200 * MB, 98_000, 4),
                                b("Archives", 5_600 * MB, 120, 75),
                                b("iOS DeviceSupport", 4_800 * MB, 9_000, 160),
                            ],
                        ),
                        d(
                            "CoreSimulator",
                            vec![
                                b("Devices", 6_200 * MB, 41_000, 260),
                                b("Caches", 3_100 * MB, 8_000, 8),
                            ],
                        ),
                    ],
                ),
                b("Mail", 2_800 * MB, 41_000, 0),
                b("Group Containers", 1_900 * MB, 3_000, 1),
                b("Logs", 410 * MB, 900, 0),
            ],
        ),
        d(
            "work",
            vec![
                d(
                    "rind",
                    vec![
                        b("target", 6_800 * MB, 21_000, 2),
                        b("crates", 4 * MB, 120, 0),
                        b("docs", 300 * KB, 10, 3),
                    ],
                ),
                d(
                    "ml-demo",
                    vec![
                        b(".venv", 2_200 * MB, 34_000, 95),
                        d(
                            "data",
                            vec![
                                f("train.parquet", 1_600 * MB, 30, 30),
                                f("test.parquet", 410 * MB, 30, 31),
                            ],
                        ),
                        b("notebooks", 12 * MB, 20, 33),
                    ],
                ),
                d(
                    "old-blog",
                    vec![
                        b("node_modules", 780 * MB, 52_000, 120),
                        b("public", 120 * MB, 900, 120),
                    ],
                ),
            ],
        ),
        d(
            "Projects",
            vec![
                d(
                    "website",
                    vec![
                        b("node_modules", 1_100 * MB, 61_000, 10),
                        b("dist", 40 * MB, 200, 10),
                    ],
                ),
                d(
                    "android-demo",
                    vec![
                        b("build", 640 * MB, 3_000, 210),
                        b("app", 80 * MB, 1_200, 210),
                    ],
                ),
            ],
        ),
        d(
            "Movies",
            vec![
                f("家庭录像 2023.mov", 8_400 * MB, 400, 300),
                f("发布会录屏.mp4", 3_200 * MB, 60, 58),
                b("Final Cut 资源", 6_100 * MB, 820, 120),
            ],
        ),
        d(
            "Downloads",
            vec![
                f("Xcode_15.4.xip", 7_200 * MB, 150, 140),
                f("ubuntu-24.04-desktop-arm64.iso", 4_700 * MB, 90, 90),
                f("WeChatMac.dmg", 410 * MB, 200, 200),
                f("飞书安装包.dmg", 380 * MB, 100, 99),
                f("年度报告.pdf", 24 * MB, 20, 3),
                b("其他小文件", 1_600 * MB, 2_300, 5),
            ],
        ),
        d(
            "Documents",
            vec![
                b("论文资料", 2_400 * MB, 900, 40),
                b("合同扫描", 1_100 * MB, 300, 70),
                f("设计稿.sketch", 680 * MB, 45, 45),
            ],
        ),
        d(
            "Pictures",
            vec![
                b("Photos Library.photoslibrary", 38_000 * MB, 52_000, 1),
                b("截图", 2_100 * MB, 1_800, 0),
            ],
        ),
        d("Music", vec![b("Music", 4_200 * MB, 3_100, 30)]),
        b("Desktop", 1_200 * MB, 340, 0),
        b(".cache", 1_400 * MB, 4_000, 2),
    ]
}

/// 示例里不能动的地方，和真实的保护区一致。
fn space_hold_back(env: &Env, path: &Path) -> Option<HoldBack> {
    let protected = |why| Some(HoldBack::Protected { why });
    let rel = path.strip_prefix(&env.home).ok()?;
    let rel = rel.to_string_lossy();
    match rel.as_ref() {
        "" | "Library" => protected(ProtectReason::RootFolder),
        "Desktop" | "Documents" | "Downloads" => protected(ProtectReason::UserFolder),
        "Library/Mail" => protected(ProtectReason::Mail),
        "Library/Application Support/MobileSync" => protected(ProtectReason::DeviceBackups),
        "Pictures/Photos Library.photoslibrary" => protected(ProtectReason::PhotoLibrary),
        "Music/Music" => protected(ProtectReason::MusicLibrary),
        _ => None,
    }
}

struct Built {
    node: SpaceNode,
    large: Vec<LargeFile>,
}

/// 把示例树变成 core 的节点。`depth` 到 `max` 的目录不展开（和 core 的 `max_depth` 一样），
/// 作为分析根的 blob 拆成几个文件和一个"其他"，免得下钻之后还是空的。
fn build(
    env: &Env,
    t: &T,
    path: PathBuf,
    depth: usize,
    max: usize,
    gone: &HashSet<PathBuf>,
) -> Built {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let hold_back = space_hold_back(env, &path);
    match t {
        T::File {
            bytes,
            modified,
            accessed,
            ..
        } => {
            let large = (*bytes >= 100 * MB)
                .then(|| LargeFile {
                    path: path.clone(),
                    size: *bytes,
                    modified: Some(days_ago(*modified)),
                    accessed: Some(days_ago(*accessed)),
                    hold_back: hold_back.clone(),
                })
                .into_iter()
                .collect();
            Built {
                node: SpaceNode {
                    path,
                    name,
                    kind: NodeKind::File,
                    size: Size::Known(*bytes),
                    files: 1,
                    partial: false,
                    modified: Some(days_ago(*modified)),
                    hold_back,
                    children: Vec::new(),
                },
                large,
            }
        }
        T::Blob {
            bytes,
            files,
            modified,
            ..
        } => {
            let children = if depth == 0 {
                blob_children(&path, *bytes, *files, *modified)
            } else {
                Vec::new()
            };
            Built {
                node: SpaceNode {
                    path,
                    name,
                    kind: NodeKind::Dir,
                    size: Size::Known(*bytes),
                    files: *files,
                    partial: false,
                    modified: Some(days_ago(*modified)),
                    hold_back,
                    children,
                },
                large: Vec::new(),
            }
        }
        T::Dir { children, .. } => {
            let mut built: Vec<Built> = children
                .iter()
                .map(|c| {
                    let child_path = path.join(child_name(c));
                    build(env, c, child_path, depth + 1, max, gone)
                })
                .filter(|c| !model::is_gone(&c.node.path, gone))
                .collect();
            built.sort_by_key(|c| std::cmp::Reverse(c.node.size.known().unwrap_or(0)));
            let size: u64 = built.iter().filter_map(|c| c.node.size.known()).sum();
            let files: u64 = built.iter().map(|c| c.node.files).sum();
            let modified = built.iter().filter_map(|c| c.node.modified).max();
            let large = built.iter_mut().flat_map(|c| c.large.drain(..)).collect();
            let children = if depth >= max {
                Vec::new()
            } else {
                built.into_iter().map(|c| c.node).collect()
            };
            Built {
                node: SpaceNode {
                    path,
                    name,
                    kind: NodeKind::Dir,
                    size: Size::Known(size),
                    files,
                    partial: false,
                    modified,
                    hold_back,
                    children,
                },
                large,
            }
        }
    }
}

fn child_name(t: &T) -> &str {
    match t {
        T::File { name, .. } | T::Blob { name, .. } | T::Dir { name, .. } => name,
    }
}

fn blob_children(path: &Path, bytes: u64, files: u64, modified: i64) -> Vec<SpaceNode> {
    let shares = [0.22, 0.15, 0.1, 0.07, 0.05];
    let mut out: Vec<SpaceNode> = shares
        .iter()
        .enumerate()
        .take(files.min(5) as usize)
        .map(|(ix, share)| {
            let child = path.join(format!("data_{ix}.bin"));
            SpaceNode {
                name: format!("data_{ix}.bin"),
                path: child,
                kind: NodeKind::File,
                size: Size::Known((bytes as f64 * share) as u64),
                files: 1,
                partial: false,
                modified: Some(days_ago(modified)),
                hold_back: None,
                children: Vec::new(),
            }
        })
        .collect();
    let used: u64 = out.iter().filter_map(|n| n.size.known()).sum();
    let rest = files.saturating_sub(out.len() as u64);
    if rest > 0 {
        out.push(SpaceNode {
            path: path.join("…"),
            name: format!("+{rest}"),
            kind: NodeKind::Other,
            size: Size::Known(bytes.saturating_sub(used)),
            files: rest,
            partial: false,
            modified: Some(days_ago(modified)),
            hold_back: None,
            children: Vec::new(),
        });
    }
    out
}

/// 在示例树里找 `rel` 对应的那一棵子树。
fn find_tree<'a>(roots: &'a [T], rel: &Path) -> Option<&'a T> {
    let mut parts = rel.components();
    let first = parts.next()?.as_os_str().to_string_lossy().into_owned();
    let mut at = roots.iter().find(|t| child_name(t) == first)?;
    for part in parts {
        let part = part.as_os_str().to_string_lossy();
        let T::Dir { children, .. } = at else {
            return None;
        };
        at = children.iter().find(|t| child_name(t) == part)?;
    }
    Some(at)
}

pub fn space(
    env: &Env,
    root: &Path,
    gone: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> SpaceReport {
    let home = home_tree();
    let built = if root == env.home {
        build(env, &d("~", home), root.to_path_buf(), 0, 3, gone)
    } else if let Some(t) = root
        .strip_prefix(&env.home)
        .ok()
        .and_then(|rel| find_tree(&home, rel))
    {
        build(env, t, root.to_path_buf(), 0, 3, gone)
    } else if root == Path::new("/Applications") {
        let apps = apps()
            .into_iter()
            .filter(|a| !a.is_system)
            .map(|a| {
                b(
                    &format!("{}.app", a.name),
                    a.size.known().unwrap_or(0),
                    1_200,
                    30,
                )
            })
            .collect();
        build(
            env,
            &d("Applications", apps),
            root.to_path_buf(),
            0,
            3,
            gone,
        )
    } else {
        let fallback = d(
            "",
            vec![
                b("资料", 3_200 * MB, 1_400, 20),
                b("归档", 1_800 * MB, 300, 200),
                f("安装包.dmg", 640 * MB, 120, 110),
                f("说明.pdf", 3 * MB, 10, 2),
            ],
        );
        build(env, &fallback, root.to_path_buf(), 0, 3, gone)
    };

    let mut seen = 0u64;
    for child in &built.node.children {
        on_progress(ScanProgress::Location(env.display(&child.path)));
        seen += child.files;
        on_progress(ScanProgress::Measuring {
            done: seen as usize,
            total: 0,
        });
        if !pause(cancel, 140) {
            break;
        }
    }
    on_progress(ScanProgress::Finished);
    let mut large = built.large;
    large.sort_by_key(|l| std::cmp::Reverse(l.size));
    let unreadable = if root == env.home {
        vec![env.home.join("Library/Containers/com.apple.Safari")]
    } else {
        Vec::new()
    };
    SpaceReport {
        root: built.node,
        large_files: large,
        unreadable,
        incomplete: cancel.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------------------------
// 启动项

fn launch_items(env: &Env) -> Vec<LaunchItem> {
    let user = |file: &str| env.home.join("Library/LaunchAgents").join(file);
    let docker = AppRef {
        name: "Docker".into(),
        bundle_id: Some("com.docker.docker".into()),
        path: PathBuf::from("/Applications/Docker.app"),
    };
    let item = |plist: PathBuf,
                domain: LaunchDomain,
                label: Option<&str>,
                program: &str,
                exists: bool,
                owner: Option<AppRef>,
                status: LaunchStatus,
                hold_back: Option<HoldBack>| LaunchItem {
        plist,
        domain,
        label: label.map(String::from),
        program: Some(PathBuf::from(program)),
        program_exists: exists,
        run_at_load: true,
        keep_alive: false,
        orphan: owner.is_none() && !exists,
        owner,
        status,
        hold_back,
    };
    use LaunchDomain::*;
    vec![
        item(user("com.xunlei.Thunder.helper.plist"), UserAgent, Some("com.xunlei.Thunder.helper"), "/Applications/Thunder.app/Contents/MacOS/helper", false, None, LaunchStatus::NotLoaded, None),
        item(user("us.zoom.updater.login.check.plist"), UserAgent, Some("us.zoom.updater.login.check"), "/Applications/zoom.us.app/Contents/Frameworks/ZoomUpdater.app/Contents/MacOS/ZoomUpdater", false, None, LaunchStatus::Disabled, None),
        item(PathBuf::from("/Library/LaunchDaemons/com.parallels.desktop.launchdaemon.plist"), GlobalDaemon, Some("com.parallels.desktop.launchdaemon"), "/Applications/Parallels Desktop.app/Contents/MacOS/prl_launch_daemon", false, None, LaunchStatus::NotLoaded, Some(HoldBack::NeedsAdmin)),
        item(user("com.docker.helper.plist"), UserAgent, Some("com.docker.helper"), "/Applications/Docker.app/Contents/Library/LaunchServices/com.docker.helper", true, Some(docker.clone()), LaunchStatus::Loaded { pid: Some(1432) }, None),
        item(user("com.google.keystone.agent.plist"), UserAgent, Some("com.google.keystone.agent"), "/Users/demo/Library/Google/GoogleSoftwareUpdate/GoogleSoftwareUpdate.bundle/Contents/Resources/GoogleSoftwareUpdateAgent.app/Contents/MacOS/GoogleSoftwareUpdateAgent", true, None, LaunchStatus::Loaded { pid: Some(611) }, None),
        item(user("com.spotify.webhelper.plist"), UserAgent, Some("com.spotify.webhelper"), "/Applications/Spotify.app/Contents/MacOS/SpotifyWebHelper", true, None, LaunchStatus::Loaded { pid: None }, None),
        item(user("broken-agent.plist"), UserAgent, None, "/usr/local/bin/sync-notes", true, None, LaunchStatus::Unknown, None),
        item(PathBuf::from("/Library/LaunchAgents/com.microsoft.update.agent.plist"), GlobalAgent, Some("com.microsoft.update.agent"), "/Library/Application Support/Microsoft/MAU2.0/Microsoft AutoUpdate.app/Contents/MacOS/Microsoft Update Assistant.app/Contents/MacOS/Microsoft Update Assistant", true, None, LaunchStatus::Loaded { pid: Some(702) }, Some(HoldBack::NeedsAdmin)),
        item(PathBuf::from("/Library/LaunchDaemons/com.docker.vmnetd.plist"), GlobalDaemon, Some("com.docker.vmnetd"), "/Library/PrivilegedHelperTools/com.docker.vmnetd", true, Some(docker), LaunchStatus::Loaded { pid: Some(301) }, Some(HoldBack::NeedsAdmin)),
        item(PathBuf::from("/Library/LaunchAgents/com.apple.SafariBookmarksSyncAgent.plist"), GlobalAgent, Some("com.apple.SafariBookmarksSyncAgent"), "/System/Library/CoreServices/SafariBookmarksSyncAgent.app/Contents/MacOS/SafariBookmarksSyncAgent", true, None, LaunchStatus::Loaded { pid: Some(498) }, Some(HoldBack::SystemOwned)),
    ]
}

// ---------------------------------------------------------------------------------------------
// 历史与状态

fn seed_history(env: &Env, lang: Lang) -> Vec<HistoryRecord> {
    let trash = env.home.join(".Trash");
    let item = |original: PathBuf, size: u64| HistoryItem {
        trashed: trash.join(original.file_name().unwrap_or_default()),
        original,
        size: Size::Known(size),
        pending: false,
        restored: false,
    };
    let home = |p: &str| env.expand(p);
    let record =
        |id: &str, hours: i64, kind, title: &str, bundle_id: Option<&str>, items, restored| {
            HistoryRecord {
                id: id.to_string(),
                at: chrono::Utc::now() - chrono::Duration::hours(hours),
                kind,
                app_name: title.to_string(),
                bundle_id: bundle_id.map(String::from),
                source: Source::App,
                items,
                restored,
            }
        };
    vec![
        record(
            "demo-4",
            3,
            RecordKind::Uninstall,
            "Spotify",
            Some("com.spotify.client"),
            vec![
                item(PathBuf::from("/Applications/Spotify.app"), 310 * MB),
                item(home("~/Library/Application Support/Spotify"), 2_900 * MB),
                item(
                    home("~/Library/Preferences/com.spotify.client.plist"),
                    12 * KB,
                ),
            ],
            false,
        ),
        record(
            "demo-3",
            26,
            RecordKind::Cleanup,
            crate::strings::ui(lang, crate::strings::Key::NavCleanup),
            None,
            vec![
                item(
                    home("~/Library/Caches/com.microsoft.VSCode.ShipIt"),
                    420 * MB,
                ),
                item(home("~/Library/Caches/node-gyp"), 96 * MB),
            ],
            false,
        ),
        record(
            "demo-2",
            50,
            RecordKind::Uninstall,
            "Zoom",
            Some("us.zoom.xos"),
            vec![
                item(PathBuf::from("/Applications/zoom.us.app"), 480 * MB),
                item(home("~/Library/Caches/us.zoom.xos"), 95 * MB),
            ],
            true,
        ),
        record(
            "demo-1",
            120,
            RecordKind::Orphans,
            "Evernote",
            Some("com.evernote.Evernote"),
            vec![item(
                home("~/Library/Containers/com.evernote.Evernote"),
                1_340 * MB,
            )],
            false,
        ),
    ]
}

/// 示例模式的"系统"：做过什么都记在这里，界面下次读时就能看到。
pub struct DemoState {
    pub history: Vec<HistoryRecord>,
    pub launch: Vec<LaunchItem>,
    /// 还在运行的 App（bundle id）。
    pub running: HashSet<String>,
    /// 已经"移到废纸篓"的路径；放回时去掉。
    pub gone: HashSet<PathBuf>,
    next_id: u64,
}

impl DemoState {
    /// `lang` 用来生成示例历史里和界面语言有关的条目（清理批次的标题），跟真实 GUI 写进
    /// ledger 的文案保持一致（`ui(lang, Key::NavCleanup)`）。
    pub fn new(env: &Env, lang: Lang) -> Self {
        Self {
            history: seed_history(env, lang),
            launch: launch_items(env),
            running: [WECHAT.to_string()].into(),
            gone: HashSet::new(),
            next_id: 10,
        }
    }

    /// 示例里的微信在不在运行（决定它的缓存是不是 `InUse`）。
    pub fn wechat_running(&self) -> bool {
        self.running.contains(WECHAT)
    }

    pub fn is_running(&self, app: &InstalledApp) -> bool {
        app.bundle_id
            .as_deref()
            .is_some_and(|id| self.running.contains(id))
    }

    pub fn quit(&mut self, app: &InstalledApp) {
        if let Some(id) = &app.bundle_id {
            self.running.remove(id);
        }
    }

    fn record(
        &mut self,
        env: &Env,
        kind: RecordKind,
        title: &str,
        bundle_id: Option<String>,
        moved: &[(PathBuf, Size)],
    ) -> Option<String> {
        if moved.is_empty() {
            return None;
        }
        self.next_id += 1;
        let id = format!("demo-{}", self.next_id);
        let trash = env.home.join(".Trash");
        self.history.insert(
            0,
            HistoryRecord {
                id: id.clone(),
                at: chrono::Utc::now(),
                kind,
                app_name: title.to_string(),
                bundle_id,
                source: Source::App,
                items: moved
                    .iter()
                    .map(|(path, size)| HistoryItem {
                        original: path.clone(),
                        trashed: trash.join(path.file_name().unwrap_or_default()),
                        size: *size,
                        pending: false,
                        restored: false,
                    })
                    .collect(),
                restored: false,
            },
        );
        self.gone.extend(moved.iter().map(|(p, _)| p.clone()));
        Some(id)
    }

    /// 模拟一次 `remove::trash_items`：被挡住的拒绝，读不了的失败，其余"移走"。
    pub fn trash(
        &mut self,
        env: &Env,
        batch: &Batch,
        candidates: &[Removable],
        selected: &HashSet<PathBuf>,
    ) -> RemovalOutcome {
        let mut outcome = RemovalOutcome::default();
        let mut moved: Vec<(PathBuf, Size)> = Vec::new();
        let mut picked: Vec<&Removable> = candidates
            .iter()
            .filter(|c| selected.contains(&c.path))
            .collect();
        // 外层先处理，里层随外层一起走。
        picked.sort_by_key(|c| c.path.components().count());
        for c in picked {
            if moved.iter().any(|(p, _)| c.path.starts_with(p)) {
                continue;
            }
            match &c.hold_back {
                Some(HoldBack::Protected { .. })
                | Some(HoldBack::NeedsAdmin)
                | Some(HoldBack::SystemOwned)
                | Some(HoldBack::InUse { .. }) => {
                    outcome.refused.push((c.path.clone(), "held back".into()))
                }
                Some(HoldBack::Unreadable) => outcome
                    .failed
                    .push((c.path.clone(), "permission denied".into())),
                _ => {
                    outcome.freed += c.size.known().unwrap_or(0);
                    moved.push((c.path.clone(), c.size));
                }
            }
        }
        let trash = env.home.join(".Trash");
        outcome.moved = moved
            .iter()
            .map(|(p, _)| (p.clone(), trash.join(p.file_name().unwrap_or_default())))
            .collect();
        outcome.record_id = self.record(
            env,
            batch.kind,
            &batch.title,
            batch.bundle_id.clone(),
            &moved,
        );
        outcome
    }

    /// 模拟卸载：App 本体和选中的残留。
    pub fn uninstall(
        &mut self,
        env: &Env,
        plan: &UninstallPlan,
        selection: &Selection,
    ) -> RemovalOutcome {
        let mut candidates: Vec<Removable> = plan
            .leftovers
            .iter()
            .map(|m| Removable {
                path: m.path.clone(),
                size: m.size,
                hold_back: m.hold_back.clone(),
            })
            .collect();
        let mut selected: HashSet<PathBuf> = selection.paths.iter().cloned().collect();
        if selection.include_app {
            candidates.insert(
                0,
                Removable {
                    path: plan.app.path.clone(),
                    size: plan.app.size,
                    hold_back: None,
                },
            );
            selected.insert(plan.app.path.clone());
        }
        let batch = Batch {
            kind: RecordKind::Uninstall,
            title: plan.app.name.clone(),
            bundle_id: plan.app.bundle_id.clone(),
            source: Source::App,
        };
        self.trash(env, &batch, &candidates, &selected)
    }

    pub fn set_launch_enabled(&mut self, item: &LaunchItem, enabled: bool) -> Result<(), String> {
        let Some(found) = self.launch.iter_mut().find(|i| i.plist == item.plist) else {
            return Err("this launch item is gone".into());
        };
        if !found.manageable() {
            return Err("this launch item cannot be managed here".into());
        }
        found.status = if enabled {
            LaunchStatus::Loaded { pid: None }
        } else {
            LaunchStatus::Disabled
        };
        Ok(())
    }

    pub fn remove_launch(
        &mut self,
        env: &Env,
        items: &[LaunchItem],
        selected: &HashSet<PathBuf>,
    ) -> RemovalOutcome {
        let candidates: Vec<Removable> = items
            .iter()
            .map(|i| Removable {
                path: i.plist.clone(),
                size: Size::Known(2 * KB),
                hold_back: if i.manageable() {
                    None
                } else {
                    Some(i.hold_back.clone().unwrap_or(HoldBack::NeedsAdmin))
                },
            })
            .collect();
        let batch = Batch {
            kind: RecordKind::LaunchItems,
            title: items
                .iter()
                .find(|i| selected.contains(&i.plist))
                .map(model::launch_name)
                .unwrap_or_default(),
            bundle_id: None,
            source: Source::App,
        };
        let outcome = self.trash(env, &batch, &candidates, selected);
        let moved: HashSet<PathBuf> = outcome.moved.iter().map(|(p, _)| p.clone()).collect();
        self.launch.retain(|i| !moved.contains(&i.plist));
        outcome
    }

    pub fn restore(&mut self, id: &str) -> RestoreOutcome {
        let mut outcome = RestoreOutcome::default();
        let Some(record) = self.history.iter_mut().find(|r| r.id == id) else {
            return outcome;
        };
        for (ix, item) in record.items.iter_mut().enumerate() {
            if item.restored || item.pending {
                continue;
            }
            // 种子里的 Spotify：第一项的原位置"已被占用"，演示部分放回。
            if id == "demo-4" && ix == 0 {
                outcome
                    .skipped
                    .push((item.original.clone(), "the original place is taken".into()));
                continue;
            }
            item.restored = true;
            self.gone.remove(&item.original);
            outcome.restored.push(item.original.clone());
        }
        record.restored = record.items.iter().all(|i| i.restored);
        outcome
    }
}
