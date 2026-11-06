//! `RIND_DEMO=1` 时的固定示例数据：几个 App、一份什么情况都有的计划、两条历史。
//! 只用来跑通和目测界面，不碰真实系统。文案一律走 `rind-i18n`，和真实模式一字不差。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rind_core::{
    AppRef, AppSource, Confidence, Env, Evidence, EvidenceKind, HistoryItem, HistoryRecord,
    HoldBack, InstalledApp, LeftoverKind, LeftoverMatch, ProtectReason, RemovalOutcome,
    RestoreOutcome, ScanProgress, Selection, Size, Source, UninstallPlan,
};
use rind_i18n::Lang;

use crate::services::{plan_view as view_of_plan, PlanView};

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

pub fn apps() -> Vec<InstalledApp> {
    let mut apps = vec![
        app(
            "Obsidian",
            "md.obsidian",
            "1.7.4",
            "6JSW4SJWN9",
            AppSource::Homebrew("obsidian".into()),
            412_000_000,
        ),
        app(
            "WeChat",
            "com.tencent.xinWeChat",
            "4.0.3",
            "5A4RE8SF68",
            AppSource::AppStore,
            735_000_000,
        ),
        app(
            "Docker",
            "com.docker.docker",
            "4.35.1",
            "9BNSXJN65R",
            AppSource::Other,
            1_980_000_000,
        ),
        app(
            "Visual Studio Code",
            "com.microsoft.VSCode",
            "1.95.2",
            "UBF8T346G9",
            AppSource::Other,
            598_000_000,
        ),
        app(
            "Figma",
            "com.figma.Desktop",
            "124.6.5",
            "T8RA8NE3B7",
            AppSource::Other,
            289_000_000,
        ),
    ];
    let mut mail = app(
        "Mail",
        "com.apple.mail",
        "16.0",
        "APPLE",
        AppSource::Other,
        30_000_000,
    );
    mail.path = PathBuf::from("/System/Applications/Mail.app");
    mail.is_system = true;
    apps.push(mail);
    apps
}

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

/// 一份把每种 confidence、hold_back、未知大小都摆出来的计划。
fn leftovers(env: &Env, app: &InstalledApp) -> Vec<LeftoverMatch> {
    let id = app.bundle_id.clone().unwrap_or_default();
    let name = app.name.clone();
    let lower = name.to_lowercase().replace(' ', "-");
    let team = app.team_id.clone().unwrap_or_default();
    let rival = AppRef {
        name: format!("{name} Insiders"),
        bundle_id: Some(format!("{id}.insiders")),
        path: PathBuf::from(format!("/Applications/{name} Insiders.app")),
    };
    let mut rows = vec![
        leftover(
            env,
            &format!("~/Library/Application Support/{lower}"),
            LeftoverKind::AppSupport,
            Size::Known(861_000_000),
            evidence(EvidenceKind::Name, Confidence::Likely, &name),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Caches/{id}"),
            LeftoverKind::Cache,
            Size::Known(1_420_000_000),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Preferences/{id}.plist"),
            LeftoverKind::Preferences,
            Size::Known(24_576),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Saved Application State/{id}.savedState"),
            LeftoverKind::SavedState,
            Size::Known(118_000),
            evidence(EvidenceKind::BundleId, Confidence::Certain, &id),
            vec![],
            None,
        ),
        leftover(
            env,
            &format!("~/Library/Logs/{name}"),
            LeftoverKind::Log,
            Size::Known(5_300_000),
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
            Size::Known(2_100_000),
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
            Size::Known(2_300_000_000),
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
            Size::Known(2_048),
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
            &format!("~/Library/Caches/Homebrew/downloads/a1b2c3--{name}.dmg"),
            LeftoverKind::HomebrewCask,
            Size::Known(183_000_000),
            evidence(
                EvidenceKind::CaskZap {
                    token: lower.clone(),
                },
                Confidence::Likely,
                &lower,
            ),
            vec![],
            None,
        ),
        leftover(
            env,
            "~/Library/Application Support/Knowledge",
            LeftoverKind::AppSupport,
            Size::Known(90_000_000),
            evidence(EvidenceKind::Name, Confidence::Possible, "Knowledge"),
            vec![],
            Some(HoldBack::SystemOwned),
        ),
    ];
    if matches!(app.source, AppSource::Other) {
        rows.push(leftover(
            env,
            &format!("/private/var/db/receipts/{id}.pkg.bom"),
            LeftoverKind::Receipt,
            Size::Known(41_000),
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
            Size::Known(640_000_000),
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

fn build(env: &Env, app: &InstalledApp) -> UninstallPlan {
    UninstallPlan {
        app: app.clone(),
        app_hold_back: None,
        leftovers: leftovers(env, app),
        running: app.bundle_id.as_deref() == Some("com.tencent.xinWeChat"),
    }
}

/// 模拟一次扫描：报几次进度，停一会儿，再给结果。
pub fn plan_view(
    env: &Env,
    app: &InstalledApp,
    lang: Lang,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> PlanView {
    for location in [
        "~/Library/Application Support",
        "~/Library/Caches",
        "~/Library/Containers",
        "/Library",
    ] {
        on_progress(ScanProgress::Location(location.to_string()));
        std::thread::sleep(Duration::from_millis(180));
    }
    let plan = build(env, app);
    let total = plan.leftovers.len();
    for done in 1..=total {
        on_progress(ScanProgress::Measuring { done, total });
        std::thread::sleep(Duration::from_millis(40));
    }
    on_progress(ScanProgress::Finished);
    view_of_plan(env, Arc::new(plan), lang)
}

pub fn execute(plan: &UninstallPlan, selection: &Selection) -> RemovalOutcome {
    let trash = PathBuf::from("/Users/demo/.Trash");
    let mut outcome = RemovalOutcome {
        record_id: Some("demo-3".to_string()),
        dry_run: false,
        ..Default::default()
    };
    if selection.include_app {
        let name = plan
            .app
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        outcome
            .moved
            .push((plan.app.path.clone(), trash.join(name)));
    }
    for m in &plan.leftovers {
        if !selection.paths.contains(&m.path) {
            continue;
        }
        let name = m
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        match &m.hold_back {
            Some(HoldBack::Protected { .. })
            | Some(HoldBack::SystemOwned)
            | Some(HoldBack::NeedsAdmin) => {
                outcome
                    .refused
                    .push((m.path.clone(), "held back".to_string()));
            }
            Some(HoldBack::Unreadable) => outcome
                .failed
                .push((m.path.clone(), "permission denied".to_string())),
            _ => outcome.moved.push((m.path.clone(), trash.join(name))),
        }
    }
    outcome.freed = plan.freed_by(selection);
    outcome
}

pub fn history() -> Vec<HistoryRecord> {
    let item = |original: &str, trashed: &str, size: u64| HistoryItem {
        original: PathBuf::from(original),
        trashed: PathBuf::from(trashed),
        size: Size::Known(size),
        pending: false,
        restored: false,
    };
    vec![
        HistoryRecord {
            id: "demo-2".to_string(),
            at: chrono::Utc::now() - chrono::Duration::hours(3),
            app_name: "Spotify".to_string(),
            bundle_id: Some("com.spotify.client".to_string()),
            source: Source::App,
            items: vec![
                item(
                    "/Applications/Spotify.app",
                    "/Users/demo/.Trash/Spotify.app",
                    310_000_000,
                ),
                item(
                    "/Users/demo/Library/Caches/com.spotify.client",
                    "/Users/demo/.Trash/com.spotify.client",
                    2_900_000_000,
                ),
                item(
                    "/Users/demo/Library/Preferences/com.spotify.client.plist",
                    "/Users/demo/.Trash/com.spotify.client.plist",
                    12_000,
                ),
            ],
            restored: false,
        },
        HistoryRecord {
            id: "demo-1".to_string(),
            at: chrono::Utc::now() - chrono::Duration::days(2),
            app_name: "Zoom".to_string(),
            bundle_id: Some("us.zoom.xos".to_string()),
            source: Source::Cli,
            items: vec![
                item(
                    "/Applications/zoom.us.app",
                    "/Users/demo/.Trash/zoom.us.app",
                    480_000_000,
                ),
                item(
                    "/Users/demo/Library/Application Support/zoom.us",
                    "/Users/demo/.Trash/zoom.us",
                    95_000_000,
                ),
            ],
            restored: true,
        },
    ]
}

pub fn restore(id: &str) -> RestoreOutcome {
    let record = history().into_iter().find(|r| r.id == id);
    let mut outcome = RestoreOutcome::default();
    if let Some(record) = record {
        let mut items = record.items.into_iter();
        if let Some(first) = items.next() {
            outcome
                .skipped
                .push((first.original, "the original place is taken".to_string()));
        }
        outcome.restored.extend(items.map(|i| i.original));
    }
    outcome
}
