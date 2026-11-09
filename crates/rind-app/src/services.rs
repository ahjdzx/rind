//! 界面和 core 之间的一层：真实模式调 core，`RIND_DEMO=1` 时换成示例后端（只改内存里的状态，
//! 不碰真实系统）。所有方法都是同步的，在后台线程上跑；拿到的都是 core 的结构化结果或整理好的行。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use gpui_kit::{Global, SharedString};
use rind_core::cleanup::{CleanupOptions, CleanupReport};
use rind_core::disk::DiskUsage;
use rind_core::history::Ledger;
use rind_core::launch::LaunchItem;
use rind_core::orphans::{OrphanOptions, OrphanReport};
use rind_core::remove::{Batch, Removable};
use rind_core::rules::RuleSet;
use rind_core::space::{SpaceOptions, SpaceReport};
use rind_core::uninstall::{ExecuteError, PlanOptions};
use rind_core::{
    AppSource, Confidence, Env, HistoryRecord, InstalledApp, RecordKind, RemovalOutcome,
    RestoreOutcome, ScanProgress, Selection, Size, Source, UninstallPlan,
};
use rind_i18n::Lang;

use crate::demo;

/// App 是从哪儿来的，给列表上的小标签用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    AppStore,
    Homebrew,
    Other,
}

/// 左栏列表里的一行。
#[derive(Clone)]
pub struct AppEntry {
    pub(crate) app: Arc<InstalledApp>,
    /// 稳定身份：App 的路径。
    pub(crate) id: SharedString,
    pub(crate) name: SharedString,
    pub(crate) version: SharedString,
    pub(crate) bundle_id: SharedString,
    pub(crate) display_path: SharedString,
    pub(crate) source: SourceKind,
}

impl AppEntry {
    fn new(env: &Env, app: InstalledApp) -> Self {
        let source = match app.source {
            AppSource::AppStore => SourceKind::AppStore,
            AppSource::Homebrew(_) => SourceKind::Homebrew,
            AppSource::Other => SourceKind::Other,
        };
        Self {
            id: app.path.to_string_lossy().to_string().into(),
            name: app.name.clone().into(),
            version: app.version.clone().unwrap_or_default().into(),
            bundle_id: app.bundle_id.clone().unwrap_or_default().into(),
            display_path: env.display(&app.path).into(),
            source,
            app: Arc::new(app),
        }
    }

    /// 搜索时匹配名字、bundle id 和路径。
    pub(crate) fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        q.is_empty()
            || self.name.to_lowercase().contains(&q)
            || self.bundle_id.to_lowercase().contains(&q)
            || self.display_path.to_lowercase().contains(&q)
    }
}

/// 残留表格里的一行。文案已经按当前语言整理好，表格只管画。
#[derive(Clone)]
pub struct LeftoverRow {
    pub(crate) path: PathBuf,
    pub(crate) display_path: SharedString,
    pub(crate) kind: SharedString,
    pub(crate) size: SharedString,
    pub(crate) bytes: Option<u64>,
    pub(crate) confidence: Confidence,
    pub(crate) reason: SharedString,
    pub(crate) hold_back: Option<SharedString>,
    pub(crate) shared_with: Vec<String>,
    pub(crate) selectable: bool,
}

/// 一次扫描的结果：core 的计划加上整理好的行。
#[derive(Clone)]
pub struct PlanView {
    pub(crate) plan: Arc<UninstallPlan>,
    pub(crate) rows: Vec<LeftoverRow>,
    pub(crate) app_size: SharedString,
    pub(crate) app_hold_back: Option<SharedString>,
    /// 最大那一项的已知大小，给大小条做分母。
    pub(crate) max_bytes: u64,
    /// 计划生成时 App 在不在运行；点「移到废纸篓」时会再查一次并更新。
    pub(crate) running: bool,
}

/// 把 core 的计划整理成表格行。示例模式和真实模式共用，文案只有这一份。
pub(crate) fn plan_view(env: &Env, plan: Arc<UninstallPlan>, lang: Lang) -> PlanView {
    let rows: Vec<LeftoverRow> = plan
        .leftovers
        .iter()
        .map(|m| LeftoverRow {
            path: m.path.clone(),
            display_path: env.display(&m.path).into(),
            kind: rind_i18n::describe_kind(lang, m.kind).into(),
            size: rind_i18n::format_size(lang, m.size).into(),
            bytes: m.size.known(),
            confidence: m.evidence.confidence,
            reason: rind_i18n::describe_evidence(lang, &m.evidence).into(),
            hold_back: m
                .hold_back
                .as_ref()
                .map(|h| rind_i18n::describe_hold_back(lang, h).into()),
            shared_with: m.rivals.iter().map(|r| r.name.clone()).collect(),
            selectable: m.selectable(),
        })
        .collect();
    PlanView {
        app_size: rind_i18n::format_size(lang, plan.app.size).into(),
        app_hold_back: plan
            .app_hold_back
            .as_ref()
            .map(|h| rind_i18n::describe_hold_back(lang, h).into()),
        max_bytes: rows.iter().filter_map(|r| r.bytes).max().unwrap_or(0),
        running: plan.running,
        rows,
        plan,
    }
}

/// 历史页里的一条。
#[derive(Clone)]
pub struct HistoryRow {
    pub(crate) id: String,
    pub(crate) kind: RecordKind,
    pub(crate) when: SharedString,
    pub(crate) title: SharedString,
    pub(crate) size: SharedString,
    pub(crate) restored: bool,
    pub(crate) items: Vec<HistoryItemRow>,
}

#[derive(Clone)]
pub struct HistoryItemRow {
    pub(crate) original: SharedString,
    pub(crate) size: SharedString,
    pub(crate) restored: bool,
    pub(crate) pending: bool,
}

/// 设置页里的一条保护区。
#[derive(Clone)]
pub struct ProtectedRow {
    pub(crate) path: SharedString,
    pub(crate) reason: SharedString,
}

/// 缓存清理的偏好：到哪儿找开发项目、多少天没动过才建议清理。只在本次会话内有效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupPrefs {
    /// 空 = 用 core 的默认目录。
    pub roots: Vec<PathBuf>,
    pub min_age_days: u32,
}

impl Default for CleanupPrefs {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            min_age_days: CleanupOptions::default().artifact_min_age_days,
        }
    }
}

impl Global for CleanupPrefs {}

/// 一次"移到废纸篓"要做什么。
#[derive(Clone)]
pub enum TrashOp {
    /// 孤儿、缓存、空间分析：走 `remove::trash_items`。
    Paths {
        batch: Batch,
        candidates: Vec<Removable>,
    },
    /// 缓存清理：动手前先 `cleanup::recheck_in_use`（扫描后才打开的程序，它的缓存不能动），再走 `remove::trash_items`。
    Cleanup {
        batch: Batch,
        report: rind_core::cleanup::CleanupReport,
    },
    /// 启动项：走 `launch::remove`（先 bootout 再进废纸篓）。
    Launch { items: Vec<LaunchItem> },
}

/// 移动没成，或只成了一半。界面按变体给人话（`strings::describe_failure`）。
#[derive(Clone, Debug)]
pub enum ExecuteFailure {
    AppRunning,
    NothingSelected,
    AppMoveFailed(String),
    /// History 打不开：Rind 一个都不会动。
    LedgerUnavailable(String),
    /// History 写到一半失败：`partial.moved` 已经在废纸篓里了。
    Partial {
        error: String,
        partial: Box<RemovalOutcome>,
    },
    Other(String),
}

impl From<ExecuteError> for ExecuteFailure {
    fn from(err: ExecuteError) -> Self {
        match err {
            ExecuteError::AppRunning => ExecuteFailure::AppRunning,
            ExecuteError::NothingSelected => ExecuteFailure::NothingSelected,
            ExecuteError::AppMoveFailed(e) => ExecuteFailure::AppMoveFailed(e),
            ExecuteError::LedgerRequired => ExecuteFailure::LedgerUnavailable(err.to_string()),
            ExecuteError::Ledger { error, partial } => ExecuteFailure::Partial { error, partial },
            other => ExecuteFailure::Other(other.to_string()),
        }
    }
}

/// 拿锁；别的线程 panic 过也照样用里面的数据，不把 panic 传到界面。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 界面用到的全部后端能力。`Arc<Services>` 在页面之间共享。
pub struct Services {
    env: Env,
    demo: bool,
    rules: OnceLock<RuleSet>,
    /// 打不开就留着错误：没有 History，Rind 不移动任何东西。
    ledger: Result<Ledger, String>,
    /// 全部已安装的 App（含 Apple 自带），匹配残留、找孤儿时用。拿锁期间扫描，避免并发重复扫。
    all_apps: Mutex<Option<Vec<InstalledApp>>>,
    /// 示例模式的"系统"：历史、启动项的状态，只活在内存里。
    demo_state: Mutex<demo::DemoState>,
}

impl Services {
    pub fn new(demo: bool) -> Self {
        let env = Env::detect();
        let ledger = if demo {
            Err("demo".to_string())
        } else {
            Ledger::open(&env).map_err(|e| format!("{e:#}"))
        };
        let demo_state = Mutex::new(demo::DemoState::new(&env));
        Self {
            env,
            demo,
            rules: OnceLock::new(),
            ledger,
            all_apps: Mutex::new(None),
            demo_state,
        }
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// History 打不开的原因；示例模式下永远是 None。
    pub fn ledger_error(&self) -> Option<&str> {
        if self.demo {
            return None;
        }
        self.ledger.as_ref().err().map(String::as_str)
    }

    /// 启动时的语言：真实模式问 i18n，示例模式固定中文。
    pub fn default_lang(&self) -> Lang {
        if self.demo {
            Lang::ZhCn
        } else {
            Lang::detect()
        }
    }

    /// 规则库（含 Homebrew cask 索引）只读一次，之后每次扫描复用。
    fn rules(&self) -> &RuleSet {
        self.rules.get_or_init(|| RuleSet::with_casks(&self.env))
    }

    fn scan_all_apps(&self) -> Vec<InstalledApp> {
        if self.demo {
            let gone = lock(&self.demo_state).gone.clone();
            demo::apps()
                .into_iter()
                .filter(|a| !gone.contains(&a.path))
                .collect()
        } else {
            rind_core::apps::scan_apps(&self.env)
        }
    }

    /// 全部已安装的 App；还没扫过就现在扫。
    fn all_apps(&self) -> Vec<InstalledApp> {
        let mut cached = lock(&self.all_apps);
        cached.get_or_insert_with(|| self.scan_all_apps()).clone()
    }

    /// 重新扫一遍已安装的 App，返回可卸载的（不含 Apple 自带），按名字排序。
    pub fn apps(&self) -> Vec<AppEntry> {
        let apps = {
            let mut cached = lock(&self.all_apps);
            let apps = self.scan_all_apps();
            *cached = Some(apps.clone());
            apps
        };
        if !self.demo {
            self.rules();
        }
        let mut entries: Vec<AppEntry> = apps
            .into_iter()
            .filter(|app| !app.is_system)
            .map(|app| AppEntry::new(&self.env, app))
            .collect();
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        entries
    }

    /// App 的图标（64px PNG）。示例模式不读也不写任何文件。
    pub fn app_icon(&self, app: &InstalledApp) -> Option<PathBuf> {
        if self.demo {
            return None;
        }
        crate::icons::icon_png(&self.env.data_dir.join("icons"), &app.path)
    }

    /// 家目录所在卷的容量。
    pub fn disk_usage(&self) -> Option<DiskUsage> {
        if self.demo {
            return Some(demo::disk());
        }
        rind_core::disk::usage(&self.env.home)
    }

    /// 出一份卸载计划，并把它整理成表格行。`cancel` 置位后扫描尽快停下，结果应丢弃。
    pub fn plan(
        &self,
        app: &InstalledApp,
        lang: Lang,
        cancel: Arc<AtomicBool>,
        on_progress: &(dyn Fn(ScanProgress) + Sync),
    ) -> PlanView {
        if self.demo {
            return demo::plan_view(&self.env, app, lang, &cancel, on_progress);
        }
        let all_apps = self.all_apps();
        let opts = PlanOptions {
            cancel: Some(cancel),
            ..PlanOptions::default()
        };
        let plan =
            rind_core::uninstall::plan(&self.env, app, &all_apps, self.rules(), &opts, on_progress);
        plan_view(&self.env, Arc::new(plan), lang)
    }

    /// 语言变了：同一份计划，重新整理文案。
    pub fn reword_plan(&self, view: &PlanView, lang: Lang) -> PlanView {
        let mut reworded = plan_view(&self.env, view.plan.clone(), lang);
        reworded.running = view.running;
        reworded
    }

    /// 现在在不在运行。点「移到废纸篓」前再问一次，不信计划时的旧值。
    pub fn is_running(&self, app: &InstalledApp) -> bool {
        if self.demo {
            return lock(&self.demo_state).is_running(app);
        }
        rind_core::running::is_running(app)
    }

    /// 按选择把 App 和残留移到废纸篓。
    pub fn execute(
        &self,
        plan: &UninstallPlan,
        selection: &Selection,
    ) -> Result<RemovalOutcome, ExecuteFailure> {
        if self.demo {
            return Ok(lock(&self.demo_state).uninstall(&self.env, plan, selection));
        }
        let ledger = self
            .ledger
            .as_ref()
            .map_err(|e| ExecuteFailure::LedgerUnavailable(e.clone()))?;
        rind_core::uninstall::execute(&self.env, plan, selection, Source::App, false, Some(ledger))
            .map_err(ExecuteFailure::from)
    }

    /// 让 App 退出。示例模式下只是把它记成"已退出"。
    pub fn quit(&self, app: &InstalledApp, force: bool) -> Result<(), String> {
        if self.demo {
            lock(&self.demo_state).quit(app);
            return Ok(());
        }
        rind_core::running::quit(app, force).map_err(|e| e.to_string())
    }

    /// 找孤儿残留。
    pub fn orphans(
        &self,
        cancel: Arc<AtomicBool>,
        on_progress: &(dyn Fn(ScanProgress) + Sync),
    ) -> OrphanReport {
        if self.demo {
            let gone = lock(&self.demo_state).gone.clone();
            return demo::orphans(&self.env, &gone, &cancel, on_progress);
        }
        let all_apps = self.all_apps();
        let opts = OrphanOptions {
            cancel: Some(cancel),
            ..OrphanOptions::default()
        };
        rind_core::orphans::scan(&self.env, &all_apps, self.rules(), &opts, on_progress)
    }

    /// 找可清理的缓存、日志、开发产物。
    pub fn cleanup(
        &self,
        prefs: &CleanupPrefs,
        cancel: Arc<AtomicBool>,
        on_progress: &(dyn Fn(ScanProgress) + Sync),
    ) -> CleanupReport {
        if self.demo {
            let (wechat, gone) = {
                let state = lock(&self.demo_state);
                (state.wechat_running(), state.gone.clone())
            };
            return demo::cleanup(&self.env, wechat, &gone, prefs, &cancel, on_progress);
        }
        let all_apps = self.all_apps();
        let opts = CleanupOptions {
            cancel: Some(cancel),
            project_roots: prefs.roots.clone(),
            artifact_min_age_days: prefs.min_age_days,
            ..CleanupOptions::default()
        };
        rind_core::cleanup::scan(&self.env, &all_apps, &opts, on_progress)
    }

    /// 没有自己设目录时，到哪些地方找开发项目。
    pub fn default_project_roots(&self) -> Vec<PathBuf> {
        if self.demo {
            return demo::project_roots(&self.env);
        }
        rind_core::cleanup::default_project_roots(&self.env)
    }

    /// 分析一个目录的空间占用。
    pub fn space(
        &self,
        root: &Path,
        cancel: Arc<AtomicBool>,
        on_progress: &(dyn Fn(ScanProgress) + Sync),
    ) -> SpaceReport {
        if self.demo {
            let gone = lock(&self.demo_state).gone.clone();
            return demo::space(&self.env, root, &gone, &cancel, on_progress);
        }
        let opts = SpaceOptions {
            cancel: Some(cancel),
            ..SpaceOptions::default()
        };
        rind_core::space::analyze(&self.env, root, &opts, on_progress)
    }

    /// 全部启动项。
    pub fn launch_items(&self) -> Vec<LaunchItem> {
        if self.demo {
            return lock(&self.demo_state).launch.clone();
        }
        rind_core::launch::list(&self.env, &self.all_apps())
    }

    /// 停用或启用一个启动项。
    pub fn set_launch_enabled(&self, item: &LaunchItem, enabled: bool) -> Result<(), String> {
        if self.demo {
            return lock(&self.demo_state).set_launch_enabled(item, enabled);
        }
        rind_core::launch::set_enabled(&self.env, item, enabled).map_err(|e| format!("{e:#}"))
    }

    /// 把选中的东西移到废纸篓，记一条 History。
    pub fn trash(
        &self,
        op: &TrashOp,
        selected: &HashSet<PathBuf>,
    ) -> Result<RemovalOutcome, ExecuteFailure> {
        if self.demo {
            let mut state = lock(&self.demo_state);
            return Ok(match op {
                TrashOp::Paths { batch, candidates } => {
                    state.trash(&self.env, batch, candidates, selected)
                }
                TrashOp::Cleanup { batch, report } => {
                    state.trash(&self.env, batch, &report.removables(), selected)
                }
                TrashOp::Launch { items } => state.remove_launch(&self.env, items, selected),
            });
        }
        let ledger = self
            .ledger
            .as_ref()
            .map_err(|e| ExecuteFailure::LedgerUnavailable(e.clone()))?;
        match op {
            TrashOp::Paths { batch, candidates } => rind_core::remove::trash_items(
                &self.env,
                batch,
                candidates,
                selected,
                false,
                Some(ledger),
            ),
            TrashOp::Cleanup { batch, report } => {
                let mut report = report.clone();
                let all_apps = self.all_apps();
                rind_core::cleanup::recheck_in_use(&self.env, &mut report, &all_apps, None);
                rind_core::remove::trash_items(
                    &self.env,
                    batch,
                    &report.removables(),
                    selected,
                    false,
                    Some(ledger),
                )
            }
            TrashOp::Launch { items } => rind_core::launch::remove(
                &self.env,
                items,
                selected,
                Source::App,
                false,
                Some(ledger),
            ),
        }
        .map_err(ExecuteFailure::from)
    }

    /// 历史记录，新的在前。
    pub fn history(&self, lang: Lang) -> Result<Vec<HistoryRow>, String> {
        let records = if self.demo {
            lock(&self.demo_state).history.clone()
        } else {
            self.ledger.as_ref()?.list().map_err(|e| e.to_string())?
        };
        Ok(records
            .iter()
            .map(|r| history_row(&self.env, r, lang))
            .collect())
    }

    /// 把一条历史记录放回去。
    pub fn restore(&self, id: &str) -> Result<RestoreOutcome, String> {
        if self.demo {
            return Ok(lock(&self.demo_state).restore(id));
        }
        let ledger = self.ledger.as_ref()?;
        ledger.restore(&self.env, id).map_err(|e| e.to_string())
    }

    /// 保护区列表。
    pub fn protected(&self, lang: Lang) -> Vec<ProtectedRow> {
        rind_core::protect::protected_roots(&self.env)
            .into_iter()
            .map(|(path, reason)| ProtectedRow {
                path: self.env.display(&path).into(),
                reason: rind_i18n::describe_protect(lang, reason).into(),
            })
            .collect()
    }
}

/// 一条记录整理成历史页的一行。新的在前由 core 保证（示例模式里也一样）。
pub(crate) fn history_row(env: &Env, record: &HistoryRecord, lang: Lang) -> HistoryRow {
    let total = record
        .items
        .iter()
        .filter_map(|i| i.size.known())
        .sum::<u64>();
    HistoryRow {
        id: record.id.clone(),
        kind: record.kind,
        when: record
            .at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
            .into(),
        title: record.app_name.clone().into(),
        size: rind_i18n::format_size(lang, Size::Known(total)).into(),
        restored: record.restored,
        items: record
            .items
            .iter()
            .map(|i| HistoryItemRow {
                original: env.display(&i.original).into(),
                size: rind_i18n::format_size(lang, i.size).into(),
                restored: i.restored,
                pending: i.pending,
            })
            .collect(),
    }
}
