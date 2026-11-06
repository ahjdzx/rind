//! 界面和 core 之间的一层：把 core 的结构化结果整理成界面直接能画的"视图模型"，
//! 并在 `RIND_DEMO=1` 时换成固定的示例数据。所有方法都是同步的，在后台线程上跑。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock, RwLock};

use gpui_kit::SharedString;
use rind_core::history::Ledger;
use rind_core::rules::RuleSet;
use rind_core::uninstall::{ExecuteError, PlanOptions};
use rind_core::{
    AppSource, Confidence, Env, HistoryRecord, InstalledApp, RemovalOutcome, RestoreOutcome,
    ScanProgress, Selection, Size, Source, UninstallPlan,
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
    pub(crate) size_known: bool,
    pub(crate) confidence: Confidence,
    pub(crate) confidence_label: SharedString,
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
    /// 计划生成时 App 在不在运行；点「移到废纸篓」时会再查一次并更新。
    pub(crate) running: bool,
}

/// 把 core 的计划整理成表格行。示例模式和真实模式共用，文案只有这一份。
pub(crate) fn plan_view(env: &Env, plan: Arc<UninstallPlan>, lang: Lang) -> PlanView {
    let rows = plan
        .leftovers
        .iter()
        .map(|m| LeftoverRow {
            path: m.path.clone(),
            display_path: env.display(&m.path).into(),
            kind: rind_i18n::describe_kind(lang, m.kind).into(),
            size: rind_i18n::format_size(lang, m.size).into(),
            size_known: matches!(m.size, Size::Known(_)),
            confidence: m.evidence.confidence,
            confidence_label: rind_i18n::describe_confidence(lang, m.evidence.confidence).into(),
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
        running: plan.running,
        rows,
        plan,
    }
}

/// 历史页里的一条。
#[derive(Clone)]
pub struct HistoryRow {
    pub(crate) id: String,
    pub(crate) when: SharedString,
    pub(crate) app_name: SharedString,
    pub(crate) items: usize,
    pub(crate) size: SharedString,
    pub(crate) restored: bool,
}

/// 设置页里的一条保护区。
#[derive(Clone)]
pub struct ProtectedRow {
    pub(crate) path: SharedString,
    pub(crate) reason: SharedString,
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
            ExecuteError::LedgerRequired => ExecuteFailure::LedgerUnavailable(err_text(&err)),
            ExecuteError::Ledger { error, partial } => ExecuteFailure::Partial { error, partial },
            other => ExecuteFailure::Other(other.to_string()),
        }
    }
}

fn err_text(err: &ExecuteError) -> String {
    err.to_string()
}

/// 界面用到的全部后端能力。`Arc<Services>` 在页面之间共享。
pub struct Services {
    env: Env,
    demo: bool,
    rules: OnceLock<RuleSet>,
    /// 打不开就留着错误：没有 History，Rind 不移动任何东西。
    ledger: Result<Ledger, String>,
    /// 最近一次扫描到的全部 App，匹配残留时用来找"还有谁在用"。
    all_apps: RwLock<Vec<InstalledApp>>,
}

impl Services {
    pub fn new(demo: bool) -> Self {
        let env = Env::detect();
        let ledger = if demo {
            Err("demo".to_string())
        } else {
            Ledger::open(&env).map_err(|e| format!("{e:#}"))
        };
        Self {
            env,
            demo,
            rules: OnceLock::new(),
            ledger,
            all_apps: RwLock::new(Vec::new()),
        }
    }

    pub fn is_demo(&self) -> bool {
        self.demo
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
        self.rules.get_or_init(|| load_rules(&self.env))
    }

    /// 已安装的 App，不含 Apple 自带的，按名字排序。
    pub fn apps(&self) -> Vec<AppEntry> {
        let apps = if self.demo {
            demo::apps()
        } else {
            rind_core::apps::scan_apps(&self.env)
        };
        if !self.demo {
            self.rules();
        }
        if let Ok(mut all) = self.all_apps.write() {
            *all = apps.clone();
        }
        let mut entries: Vec<AppEntry> = apps
            .into_iter()
            .filter(|app| !app.is_system)
            .map(|app| AppEntry::new(&self.env, app))
            .collect();
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        entries
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
            return demo::plan_view(&self.env, app, lang, on_progress);
        }
        let all_apps = self
            .all_apps
            .read()
            .map(|all| all.clone())
            .unwrap_or_default();
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
            return app.bundle_id.as_deref() == Some("com.tencent.xinWeChat");
        }
        rind_core::running::is_running(app)
    }

    /// 按选择移到废纸篓。
    pub fn execute(
        &self,
        plan: &UninstallPlan,
        selection: &Selection,
    ) -> Result<RemovalOutcome, ExecuteFailure> {
        if self.demo {
            return Ok(demo::execute(plan, selection));
        }
        let ledger = self
            .ledger
            .as_ref()
            .map_err(|e| ExecuteFailure::LedgerUnavailable(e.clone()))?;
        rind_core::uninstall::execute(&self.env, plan, selection, Source::App, false, Some(ledger))
            .map_err(ExecuteFailure::from)
    }

    /// 让 App 退出。
    pub fn quit(&self, app: &InstalledApp, force: bool) -> Result<(), String> {
        if self.demo {
            return Ok(());
        }
        rind_core::running::quit(app, force).map_err(|e| e.to_string())
    }

    /// 历史记录，新的在前。
    pub fn history(&self, lang: Lang) -> Result<Vec<HistoryRow>, String> {
        let records = if self.demo {
            demo::history()
        } else {
            self.ledger.as_ref()?.list().map_err(|e| e.to_string())?
        };
        Ok(records.iter().map(|r| history_row(r, lang)).collect())
    }

    /// 把一条历史记录放回去。
    pub fn restore(&self, id: &str) -> Result<RestoreOutcome, String> {
        if self.demo {
            return Ok(demo::restore(id));
        }
        let ledger = self.ledger.as_ref()?;
        restore_record(ledger, &self.env, id).map_err(|e| e.to_string())
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

fn history_row(record: &HistoryRecord, lang: Lang) -> HistoryRow {
    let total = record
        .items
        .iter()
        .filter_map(|i| i.size.known())
        .sum::<u64>();
    HistoryRow {
        id: record.id.clone(),
        when: record
            .at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
            .into(),
        app_name: record.app_name.clone().into(),
        items: record.items.len(),
        size: rind_i18n::format_size(lang, Size::Known(total)).into(),
        restored: record.restored,
    }
}

/// core 的规则库入口。cask 索引随规则一起只读一次。
fn load_rules(env: &Env) -> RuleSet {
    RuleSet::with_casks(env)
}

/// core 的放回入口。
fn restore_record(ledger: &Ledger, env: &Env, id: &str) -> anyhow::Result<RestoreOutcome> {
    ledger.restore(env, id)
}
