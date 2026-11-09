//! 应用页：左栏选 App，右栏看它留下了什么、选哪些、移到废纸篓。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IndexPath, Sizable as _,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::{RemovalOutcome, ScanProgress, Selection, Size};

use crate::services::{AppEntry, ExecuteFailure, PlanView, Services, SourceKind};
use crate::store::Scans;
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self as kit, Tone};

fn source_label(lang: Lang, source: SourceKind) -> &'static str {
    match source {
        SourceKind::AppStore => ui(lang, Key::SourceAppStore),
        SourceKind::Homebrew => ui(lang, Key::SourceHomebrew),
        SourceKind::Other => ui(lang, Key::SourceOther),
    }
}

/// 左栏的 App 列表：过滤和一行怎么画。选中由 `ListState` 自己管。
pub struct AppList {
    all: Vec<AppEntry>,
    visible: Vec<usize>,
    query: String,
    lang: Lang,
    /// 已经转好的图标（App id → PNG）。
    icons: HashMap<SharedString, PathBuf>,
}

impl AppList {
    fn new(lang: Lang) -> Self {
        Self {
            all: Vec::new(),
            visible: Vec::new(),
            query: String::new(),
            lang,
            icons: HashMap::new(),
        }
    }

    fn set_apps(&mut self, apps: Vec<AppEntry>) {
        self.all = apps;
        self.apply_filter();
    }

    fn apply_filter(&mut self) {
        let query = self.query.clone();
        self.visible = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, e)| e.matches(&query))
            .map(|(i, _)| i)
            .collect();
    }

    fn entry_at(&self, ix: IndexPath) -> Option<&AppEntry> {
        self.visible.get(ix.row).and_then(|&i| self.all.get(i))
    }
}

impl ListDelegate for AppList {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.apply_filter();
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.visible.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let entry = self.entry_at(ix)?;
        let muted = cx.theme().muted_foreground;
        let icon = self.icons.get(&entry.id).cloned();
        let meta = format!(
            "{} · {}",
            if entry.version.is_empty() {
                "—"
            } else {
                entry.version.as_ref()
            },
            rind_i18n::format_size(self.lang, entry.app.size)
        );
        let source = source_label(self.lang, entry.source);
        Some(
            ListItem::new(ElementId::from(entry.id.clone())).child(
                h_flex()
                    .w_full()
                    .gap_2p5()
                    .px_2()
                    .py_1()
                    .child(kit::app_avatar(&entry.name, icon, px(32.)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(entry.name.clone()))
                            .child(div().text_xs().text_color(muted).truncate().child(meta)),
                    )
                    .child(Tag::secondary().xsmall().child(source)),
            ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .p_4()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(ui(self.lang, Key::AppsEmpty))
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
    }
}

/// 残留表格：一份计划的行，和用户的选择。选择只在这里有一份。
pub struct LeftoverTable {
    lang: Lang,
    view: Option<PlanView>,
    selection: Selection,
    loading: bool,
    columns: Vec<Column>,
}

impl LeftoverTable {
    fn new(lang: Lang) -> Self {
        Self {
            lang,
            view: None,
            selection: Selection::default(),
            loading: false,
            columns: Self::columns(lang),
        }
    }

    fn columns(lang: Lang) -> Vec<Column> {
        vec![
            Column::new("select", "").width(px(40.)).resizable(false),
            Column::new("path", ui(lang, Key::ColPath)).width(px(380.)),
            Column::new("kind", ui(lang, Key::ColKind)).width(px(110.)),
            Column::new("size", ui(lang, Key::ColSize))
                .width(px(110.))
                .text_right(),
            Column::new("confidence", ui(lang, Key::ColConfidence)).width(px(90.)),
            Column::new("reason", ui(lang, Key::ColReason)).width(px(360.)),
        ]
    }

    fn view(&self) -> Option<&PlanView> {
        self.view.as_ref()
    }

    fn selection(&self) -> &Selection {
        &self.selection
    }

    fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    fn set_view(&mut self, view: Option<PlanView>) {
        self.selection = view
            .as_ref()
            .map(|v| v.plan.suggested_selection())
            .unwrap_or_default();
        self.view = view;
        self.loading = false;
    }

    /// 刚查过：App 现在在不在运行。
    fn set_running(&mut self, running: bool) {
        if let Some(view) = self.view.as_mut() {
            view.running = running;
        }
    }

    /// 换语言：行的文案换掉，选择照旧。
    fn reword(&mut self, view: PlanView, lang: Lang) {
        self.lang = lang;
        self.columns = Self::columns(lang);
        self.view = Some(view);
    }

    fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
        self.columns = Self::columns(lang);
    }

    fn set_selected(&mut self, path: &Path, selected: bool) {
        let allowed = self
            .view
            .as_ref()
            .is_some_and(|v| v.rows.iter().any(|r| r.path == path && r.selectable));
        if !allowed {
            return;
        }
        if selected {
            self.selection.paths.insert(path.to_path_buf());
        } else {
            self.selection.paths.remove(path);
        }
    }

    fn select_suggested(&mut self) {
        if let Some(view) = &self.view {
            self.selection = view.plan.suggested_selection();
        }
    }

    fn clear_selection(&mut self) {
        self.selection = Selection::default();
    }

    fn set_include_app(&mut self, include: bool) {
        let allowed = self
            .view
            .as_ref()
            .is_some_and(|v| v.plan.app_hold_back.is_none());
        self.selection.include_app = include && allowed;
    }

    fn selected_count(&self) -> usize {
        self.selection.paths.len() + usize::from(self.selection.include_app)
    }

    fn freed(&self) -> u64 {
        self.view
            .as_ref()
            .map_or(0, |v| v.plan.freed_by(&self.selection))
    }

    /// 选中了但大小未知的项数。
    fn unknown_selected(&self) -> usize {
        self.view.as_ref().map_or(0, |v| {
            v.rows
                .iter()
                .filter(|r| r.bytes.is_none() && self.selection.paths.contains(&r.path))
                .count()
        })
    }
}

impl TableDelegate for LeftoverTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.view.as_ref().map_or(0, |v| v.rows.len())
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .size_full()
            .justify_center()
            .items_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(ui(self.lang, Key::LeftoversEmpty))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.view.as_ref().and_then(|v| v.rows.get(row_ix)) else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        match col_ix {
            0 => {
                let checked = self.selection.paths.contains(&row.path);
                let path = row.path.clone();
                Checkbox::new(ElementId::from(row.display_path.clone()))
                    .checked(checked)
                    .disabled(!row.selectable)
                    .on_change(cx.listener(move |table, checked: &bool, _, cx| {
                        table.delegate_mut().set_selected(&path, *checked);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            1 => div()
                .text_sm()
                .truncate()
                .when(!row.selectable, |this| this.text_color(muted))
                .child(row.display_path.clone())
                .into_any_element(),
            2 => div()
                .text_sm()
                .text_color(muted)
                .child(row.kind.clone())
                .into_any_element(),
            3 => {
                let max = self.view.as_ref().map_or(0, |v| v.max_bytes);
                let fraction = match (row.bytes, max) {
                    (Some(n), m) if m > 0 => n as f32 / m as f32,
                    _ => 0.0,
                };
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_right()
                            .when(row.bytes.is_none(), |this| this.text_color(muted))
                            .child(row.size.clone()),
                    )
                    .child(kit::bar(fraction, cx.theme().primary.opacity(0.7), cx).h(px(4.)))
                    .into_any_element()
            }
            4 => kit::confidence_tag(self.lang, row.confidence).into_any_element(),
            _ => {
                if let Some(hold_back) = row.hold_back.clone() {
                    h_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(LucideIcon::TriangleAlert).xsmall())
                        .child(div().truncate().child(hold_back))
                        .into_any_element()
                } else if row.shared_with.is_empty() {
                    div()
                        .text_sm()
                        .truncate()
                        .child(row.reason.clone())
                        .into_any_element()
                } else {
                    let shared = strings::shared_with(self.lang, &row.shared_with);
                    div()
                        .text_sm()
                        .truncate()
                        .child(format!("{}，{}", row.reason, shared))
                        .into_any_element()
                }
            }
        }
    }
}

/// 扫描到哪一步了。
enum ScanState {
    Idle,
    Scanning(Option<ScanProgress>),
    Ready,
}

pub struct AppsPage {
    services: Arc<Services>,
    scans: Entity<Scans>,
    list: Entity<ListState<AppList>>,
    table: Entity<TableState<LeftoverTable>>,
    selected: Option<AppEntry>,
    scan: ScanState,
    /// 每次开始扫描加一，晚到的结果对不上就丢掉。
    generation: u64,
    /// 正在跑的那次扫描的取消标志；换 App 或重扫时置位，让它尽快停。
    cancel: Option<Arc<AtomicBool>>,
    /// 正在移动或退出 App，期间按钮不可用。
    busy: bool,
    /// 最近一次装进列表的 App 列表（看 epoch 判断有没有新的）。
    apps_epoch: u64,
    /// 已经请求过图标的 App，避免重复转换。
    icons_requested: HashSet<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl AppsPage {
    pub fn new(
        services: Arc<Services>,
        scans: Entity<Scans>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lang = strings::lang(cx);
        let list = cx.new(|cx| ListState::new(AppList::new(lang), window, cx).searchable(true));
        let table = cx.new(|cx| {
            TableState::new(LeftoverTable::new(lang), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
        });
        let subscriptions = vec![
            cx.subscribe(&list, |this, _, event: &ListEvent, cx| match event {
                ListEvent::Select(ix) | ListEvent::Confirm(ix) => this.select_at(*ix, cx),
                ListEvent::Cancel => {}
            }),
            cx.observe(&table, |_, _, cx| cx.notify()),
            cx.observe(&scans, |this, _, cx| this.sync_apps(cx)),
            cx.observe_global::<LangSetting>(|this, cx| this.lang_changed(cx)),
        ];
        let mut page = Self {
            services,
            scans,
            list,
            table,
            selected: None,
            scan: ScanState::Idle,
            generation: 0,
            cancel: None,
            busy: false,
            apps_epoch: 0,
            icons_requested: HashSet::new(),
            _subscriptions: subscriptions,
        };
        page.sync_apps(cx);
        page
    }

    fn apps_loading(&self, cx: &App) -> bool {
        let slot = &self.scans.read(cx).apps;
        slot.is_running() && slot.data.is_none()
    }

    fn load_apps(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.load_apps(cx));
    }

    /// 共享仓库里的 App 列表变了（新扫描或剪枝）：装进列表，选中的 App 不在了就清掉详情。
    fn sync_apps(&mut self, cx: &mut Context<Self>) {
        let (epoch, apps) = {
            let slot = &self.scans.read(cx).apps;
            (slot.epoch, slot.data.clone())
        };
        let Some(apps) = apps else {
            cx.notify();
            return;
        };
        let current_len = self.list.read(cx).delegate().all.len();
        if epoch == self.apps_epoch && current_len == apps.len() {
            cx.notify();
            return;
        }
        self.apps_epoch = epoch;
        let apps: Vec<AppEntry> = apps.as_ref().clone();
        if let Some(selected) = &self.selected {
            if !apps.iter().any(|a| a.id == selected.id) {
                self.selected = None;
                self.scan = ScanState::Idle;
                self.stop_scan();
                self.table.update(cx, |table, cx| {
                    table.delegate_mut().set_view(None);
                    table.refresh(cx);
                    cx.notify();
                });
            }
        }
        self.load_icons(&apps, cx);
        self.list.update(cx, |list, cx| {
            list.delegate_mut().set_apps(apps);
            cx.notify();
        });
        cx.notify();
    }

    /// 在后台一个个转图标，转好一个显示一个；失败的就一直用字母头像。
    fn load_icons(&mut self, apps: &[AppEntry], cx: &mut Context<Self>) {
        let missing: Vec<AppEntry> = apps
            .iter()
            .filter(|a| self.icons_requested.insert(a.id.clone()))
            .cloned()
            .collect();
        if missing.is_empty() || self.services.is_demo() {
            return;
        }
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<(SharedString, PathBuf)>();
            let task = cx.background_spawn(async move {
                for entry in missing {
                    if let Some(png) = services.app_icon(&entry.app) {
                        let _ = tx.unbounded_send((entry.id.clone(), png));
                    }
                }
            });
            while let Some((id, png)) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.list.update(cx, |list, cx| {
                        list.delegate_mut().icons.insert(id, png);
                        cx.notify();
                    });
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            task.await;
        })
        .detach();
    }

    /// 选了一个 App，或再点了一次同一个：都重新扫描，这样退出 App 之后再点就能看到最新状态。
    fn select_at(&mut self, ix: IndexPath, cx: &mut Context<Self>) {
        let Some(entry) = self.list.read(cx).delegate().entry_at(ix).cloned() else {
            return;
        };
        self.selected = Some(entry);
        self.start_plan(cx);
    }

    /// 让上一次扫描停下来。结果反正会被 generation 挡掉，这里是省它的时间和线程。
    fn stop_scan(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        self.stop_scan();
        self.generation += 1;
        self.scan = ScanState::Idle;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_loading(false);
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn start_plan(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        let lang = strings::lang(cx);
        self.stop_scan();
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.scan = ScanState::Scanning(None);
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_view(None);
            table.delegate_mut().set_loading(true);
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();

        let services = self.services.clone();
        let app = entry.app.clone();
        cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<ScanProgress>();
            let work = cx.background_spawn(async move {
                let report = move |progress: ScanProgress| {
                    let _ = tx.unbounded_send(progress);
                };
                services.plan(&app, lang, cancel, &report)
            });
            while let Some(progress) = rx.next().await {
                if this
                    .update(cx, |this, cx| {
                        this.scan_progressed(generation, progress, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
            let view = work.await;
            this.update(cx, |this, cx| this.plan_ready(generation, view, cx))
                .ok();
        })
        .detach();
    }

    fn scan_progressed(&mut self, generation: u64, progress: ScanProgress, cx: &mut Context<Self>) {
        if generation != self.generation || matches!(progress, ScanProgress::Finished) {
            return;
        }
        self.scan = ScanState::Scanning(Some(progress));
        cx.notify();
    }

    fn plan_ready(&mut self, generation: u64, view: PlanView, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.cancel = None;
        self.scan = ScanState::Ready;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_view(Some(view));
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn lang_changed(&mut self, cx: &mut Context<Self>) {
        let lang = strings::lang(cx);
        self.list.update(cx, |list, cx| {
            list.delegate_mut().lang = lang;
            cx.notify();
        });
        let reworded = self
            .table
            .read(cx)
            .delegate()
            .view()
            .map(|view| self.services.reword_plan(view, lang));
        self.table.update(cx, |table, cx| {
            match reworded {
                Some(view) => table.delegate_mut().reword(view, lang),
                None => table.delegate_mut().set_lang(lang),
            }
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn select_suggested(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().select_suggested();
            cx.notify();
        });
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            cx.notify();
        });
    }

    fn set_include_app(&mut self, include: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_include_app(include);
            cx.notify();
        });
    }

    fn quit_app(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let services = self.services.clone();
        let app = entry.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { services.quit(&app, force) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.start_plan(cx),
                    Err(message) => {
                        let lang = strings::lang(cx);
                        window.push_notification(
                            Notification::error(strings::error(lang, &message)),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 点了「移到废纸篓」：先在后台再查一次 App 在不在运行（不信计划时的旧值），再问。
    fn confirm_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.clone() else {
            return;
        };
        if self.busy || self.services.ledger_error().is_some() {
            return;
        }
        let selected = self.table.read(cx).delegate().selected_count();
        if selected == 0 {
            return;
        }
        self.busy = true;
        cx.notify();
        let services = self.services.clone();
        let app = entry.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let running = cx
                .background_spawn(async move { services.is_running(&app) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.table.update(cx, |table, cx| {
                    table.delegate_mut().set_running(running);
                    cx.notify();
                });
                if running {
                    let lang = strings::lang(cx);
                    window.push_notification(
                        Notification::warning(strings::app_running(lang, &entry.name)),
                        cx,
                    );
                } else {
                    this.open_confirm_dialog(window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = strings::lang(cx);
        let lines = {
            let delegate = self.table.read(cx).delegate();
            let Some(view) = delegate.view() else { return };
            let selection = delegate.selection();
            let count = delegate.selected_count();
            if count == 0 {
                return;
            }
            let freed = rind_i18n::format_size(lang, Size::Known(delegate.freed()));
            let unknown = delegate.unknown_selected();
            let mut lines: Vec<SharedString> =
                vec![strings::confirm_items(lang, count, &freed).into()];
            lines.push(if selection.include_app {
                strings::confirm_with_app(lang, &view.plan.app.name).into()
            } else {
                ui(lang, Key::ConfirmWithoutApp).into()
            });
            if unknown > 0 {
                lines.push(strings::confirm_unknown(lang, unknown).into());
            }
            lines.push(ui(lang, Key::ConfirmNote).into());
            lines.push(ui(lang, Key::EmptyTrashHint).into());
            lines
        };
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .confirm()
                .title(ui(lang, Key::ConfirmTitle))
                .description(
                    v_flex()
                        .gap_1()
                        .text_sm()
                        .children(lines.iter().cloned().map(|line| div().child(line))),
                )
                .ok_text(ui(lang, Key::MoveToTrash))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(ui(lang, Key::Cancel))
                .on_ok(move |_, window, cx| {
                    weak.update(cx, |this, cx| this.execute(window, cx)).ok();
                    true
                })
        });
    }

    fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (plan, selection) = {
            let delegate = self.table.read(cx).delegate();
            let Some(view) = delegate.view() else {
                return;
            };
            (view.plan.clone(), delegate.selection().clone())
        };
        self.busy = true;
        cx.notify();
        let services = self.services.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { services.execute(&plan, &selection) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.execute_finished(result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn execute_finished(
        &mut self,
        result: Result<RemovalOutcome, ExecuteFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        let lang = strings::lang(cx);
        if let Err(ExecuteFailure::AppRunning) = result {
            self.table.update(cx, |table, cx| {
                table.delegate_mut().set_running(true);
                cx.notify();
            });
            let name = self
                .selected
                .as_ref()
                .map(|s| s.name.to_string())
                .unwrap_or_default();
            window.push_notification(
                Notification::warning(strings::app_running(lang, &name))
                    .title(ui(lang, Key::NothingMoved)),
                cx,
            );
        } else if let Some(outcome) = kit::notify_outcome(&result, window, cx) {
            self.after_moving(&outcome, cx);
        }
        cx.notify();
    }

    /// 移动过东西之后：从所有共享结果里剪掉；App 本体走了就清掉详情，否则重新扫描；列表也重读。
    fn after_moving(&mut self, outcome: &RemovalOutcome, cx: &mut Context<Self>) {
        let moved: HashSet<PathBuf> = outcome.moved.iter().map(|(p, _)| p.clone()).collect();
        self.scans.update(cx, |s, cx| s.forget(&moved, cx));
        let app_path = self.selected.as_ref().map(|s| s.app.path.clone());
        let app_moved = app_path.is_some_and(|p| moved.contains(&p));
        if app_moved {
            self.selected = None;
            self.scan = ScanState::Idle;
            self.stop_scan();
            self.table.update(cx, |table, cx| {
                table.delegate_mut().set_view(None);
                table.refresh(cx);
                cx.notify();
            });
        } else {
            self.start_plan(cx);
        }
        self.load_apps(cx);
    }

    fn render_app_list(&self, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let loading = self.apps_loading(cx);
        let count = self.list.read(cx).delegate().all.len();
        v_flex()
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .px_3()
                    .pt_1()
                    .pb_1()
                    .text_xs()
                    .text_color(muted)
                    .child(div().flex_1().child(strings::app_count_line(lang, count)))
                    .child(
                        Button::new("refresh-apps")
                            .xsmall()
                            .ghost()
                            .icon(Icon::new(LucideIcon::RefreshCw))
                            .tooltip(ui(lang, Key::RefreshApps))
                            .loading(self.scans.read(cx).apps.is_running())
                            .on_click(cx.listener(|this, _, _, cx| this.load_apps(cx))),
                    ),
            )
            .map(|this| {
                if loading {
                    this.child(
                        h_flex()
                            .gap_2()
                            .p_3()
                            .text_sm()
                            .text_color(muted)
                            .child(Spinner::new().small())
                            .child(ui(lang, Key::AppsLoading)),
                    )
                } else {
                    this.child(
                        div().flex_1().min_h_0().child(
                            List::new(&self.list).search_placeholder(ui(lang, Key::SearchApps)),
                        ),
                    )
                }
            })
    }

    fn render_header(&self, entry: &AppEntry, lang: Lang, cx: &Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // 只拿需要的那个字段，不把整份计划 clone 一遍。
        let size: SharedString = self
            .table
            .read(cx)
            .delegate()
            .view()
            .map(|v| v.app_size.clone())
            .unwrap_or_default();
        let icon = self.list.read(cx).delegate().icons.get(&entry.id).cloned();
        h_flex()
            .gap_3()
            .px_5()
            .pt_4()
            .pb_3()
            .child(kit::app_avatar(&entry.name, icon, px(48.)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .truncate()
                                    .child(entry.name.clone()),
                            )
                            .child(
                                Tag::secondary()
                                    .xsmall()
                                    .child(source_label(lang, entry.source)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .text_xs()
                            .text_color(muted)
                            .child(format!(
                                "{}: {}",
                                ui(lang, Key::DetailBundleId),
                                entry.bundle_id
                            ))
                            .child(format!(
                                "{}: {}",
                                ui(lang, Key::DetailVersion),
                                entry.version
                            ))
                            .when(!size.is_empty(), |this| {
                                this.child(format!("{}: {}", ui(lang, Key::DetailSize), size))
                            }),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(format!(
                        "{}: {}",
                        ui(lang, Key::DetailPath),
                        entry.display_path
                    ))),
            )
    }

    fn render_notices(
        &self,
        entry: &AppEntry,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (running, app_hold_back) = {
            let delegate = self.table.read(cx).delegate();
            let view = delegate.view();
            (
                view.is_some_and(|v| v.running),
                view.and_then(|v| v.app_hold_back.clone()),
            )
        };
        let ledger_error = self
            .services
            .ledger_error()
            .map(|e| strings::ledger_unavailable(lang, e));
        v_flex()
            .px_5()
            .gap_2()
            .when_some(ledger_error, |this, message| {
                this.child(kit::banner(
                    Tone::Danger,
                    LucideIcon::TriangleAlert,
                    message,
                    cx,
                ))
            })
            .when(running, |this| {
                this.child(kit::banner(
                    Tone::Warning,
                    LucideIcon::TriangleAlert,
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(strings::app_running(lang, &entry.name)),
                        )
                        .child(
                            Button::new("quit-app")
                                .xsmall()
                                .outline()
                                .label(ui(lang, Key::Quit))
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.quit_app(false, window, cx)
                                })),
                        )
                        .child(
                            Button::new("force-quit-app")
                                .xsmall()
                                .danger()
                                .label(ui(lang, Key::ForceQuit))
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.quit_app(true, window, cx)
                                })),
                        ),
                    cx,
                ))
            })
            .when_some(app_hold_back, |this, reason| {
                this.child(kit::banner(
                    Tone::Warning,
                    LucideIcon::ShieldCheck,
                    strings::app_held(lang, &reason),
                    cx,
                ))
            })
    }

    fn render_toolbar(&self, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let (has_view, app_locked, include_app) = {
            let delegate = self.table.read(cx).delegate();
            (
                delegate.view().is_some(),
                delegate.view().is_some_and(|v| v.app_hold_back.is_some()),
                delegate.selection().include_app,
            )
        };
        let ready = matches!(self.scan, ScanState::Ready) && has_view;
        let scanning = matches!(self.scan, ScanState::Scanning(_));
        h_flex()
            .gap_2()
            .px_5()
            .py_3()
            .flex_wrap()
            .child(if scanning {
                Button::new("cancel-scan")
                    .small()
                    .outline()
                    .icon(Icon::new(LucideIcon::X))
                    .label(ui(lang, Key::Cancel))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_scan(cx)))
            } else {
                Button::new("rescan")
                    .small()
                    .outline()
                    .icon(Icon::new(LucideIcon::RefreshCw))
                    .label(ui(lang, Key::Rescan))
                    .disabled(self.busy || self.selected.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.start_plan(cx)))
            })
            .child(
                Button::new("select-suggested")
                    .small()
                    .outline()
                    .icon(Icon::new(LucideIcon::Sparkles))
                    .label(ui(lang, Key::SelectSuggested))
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, _, cx| this.select_suggested(cx))),
            )
            .child(
                Button::new("clear-selection")
                    .small()
                    .ghost()
                    .label(ui(lang, Key::ClearSelection))
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            )
            .child(div().flex_1())
            .child(
                Checkbox::new("include-app")
                    .label(ui(lang, Key::IncludeApp))
                    .checked(include_app)
                    .disabled(!ready || app_locked)
                    .on_change(cx.listener(|this, include: &bool, _, cx| {
                        this.set_include_app(*include, cx)
                    })),
            )
    }

    fn render_footer(&self, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let (has_view, running, count, freed, unknown) = {
            let delegate = self.table.read(cx).delegate();
            (
                delegate.view().is_some(),
                delegate.view().is_some_and(|v| v.running),
                delegate.selected_count(),
                delegate.freed(),
                delegate.unknown_selected(),
            )
        };
        let ready = matches!(self.scan, ScanState::Ready) && has_view;
        let no_ledger = self.services.ledger_error().is_some();
        kit::footer_bar(cx)
            .child(kit::picked_summary(
                lang,
                crate::model::Picked {
                    count,
                    bytes: freed,
                    unknown,
                },
                cx,
            ))
            .child(
                Button::new("move-to-trash")
                    .danger()
                    .icon(Icon::new(LucideIcon::Trash))
                    .label(ui(lang, Key::MoveToTrash))
                    .loading(self.busy)
                    .disabled(!ready || count == 0 || running || self.busy || no_ledger)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_trash(window, cx))),
            )
    }

    fn render_detail(&self, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entry) = self.selected.clone() else {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(kit::empty_state(
                    LucideIcon::AppWindow,
                    ui(lang, Key::NoAppSelectedTitle),
                    ui(lang, Key::NoAppSelected),
                    cx,
                ))
                .into_any_element();
        };
        let progress = match &self.scan {
            ScanState::Scanning(progress) => Some(progress.clone()),
            _ => None,
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_header(&entry, lang, cx))
            .child(self.render_notices(&entry, lang, cx))
            .child(self.render_toolbar(lang, cx))
            .when_some(progress, |this, progress| {
                this.child(div().px_5().pb_3().child(kit::progress_block(
                    "plan-progress",
                    lang,
                    progress.as_ref(),
                    cx,
                )))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_5()
                    .pb_4()
                    .child(DataTable::new(&self.table).stripe(true).bordered(true)),
            )
            .child(self.render_footer(lang, cx))
            .into_any_element()
    }
}

impl Render for AppsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let header = kit::page_header(
            LucideIcon::AppWindow,
            ui(lang, Key::NavApps),
            ui(lang, Key::AppsSubtitle),
            div(),
            cx,
        );
        v_flex().size_full().child(header).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(self.render_app_list(lang, cx))
                .child(self.render_detail(lang, cx)),
        )
    }
}
