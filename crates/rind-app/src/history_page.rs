//! 历史页：Rind 移走过什么（卸载、孤儿、缓存、文件、启动项），展开看每一项，一键放回。

use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::{RecordKind, RestoreOutcome};

use crate::services::{HistoryRow, Services};
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone};

fn kind_tag(lang: Lang, kind: RecordKind) -> Tag {
    let tag = match kind {
        RecordKind::Uninstall => Tag::info(),
        RecordKind::Orphans => Tag::warning(),
        RecordKind::Cleanup => Tag::success(),
        RecordKind::Files => Tag::secondary(),
        RecordKind::LaunchItems => Tag::danger(),
    };
    tag.xsmall()
        .child(rind_i18n::describe_record_kind(lang, kind))
}

pub struct HistoryPage {
    services: Arc<Services>,
    rows: Vec<HistoryRow>,
    loading: bool,
    error: Option<SharedString>,
    expanded: HashSet<String>,
    /// 正在放回的那条记录。
    restoring: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl HistoryPage {
    pub fn new(services: Arc<Services>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe_global::<LangSetting>(|this, cx| this.refresh(cx))];
        let mut page = Self {
            services,
            rows: Vec::new(),
            loading: false,
            error: None,
            expanded: HashSet::new(),
            restoring: None,
            _subscriptions: subscriptions,
        };
        page.refresh(cx);
        page
    }

    /// 重新读历史。切到这一页、换语言、放回之后都会调。
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        let services = self.services.clone();
        let lang = strings::lang(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { services.history(lang) })
                .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(rows) => {
                        this.rows = rows;
                        this.error = None;
                    }
                    Err(message) => {
                        this.error = Some(strings::error(strings::lang(cx), &message).into())
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restore(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.restoring.is_some() {
            return;
        }
        self.restoring = Some(id.clone());
        cx.notify();
        let services = self.services.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { services.restore(&id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.restore_finished(result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn restore_finished(
        &mut self,
        result: Result<RestoreOutcome, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restoring = None;
        let lang = strings::lang(cx);
        match result {
            Ok(outcome) => {
                let summary =
                    strings::restore_summary(lang, outcome.restored.len(), outcome.skipped.len());
                let note = if outcome.restored.is_empty() {
                    Notification::warning(summary)
                } else {
                    Notification::success(summary).title(ui(lang, Key::Restored))
                };
                window.push_notification(note, cx);
            }
            Err(message) => {
                window.push_notification(Notification::error(strings::error(lang, &message)), cx)
            }
        }
        self.refresh(cx);
    }

    fn render_row(&self, row: &HistoryRow, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = ui::muted(cx);
        let expanded = self.expanded.contains(&row.id);
        let toggle_id = row.id.clone();
        let restore_id = row.id.clone();
        let meta = format!(
            "{}  ·  {}  ·  {}",
            row.when,
            strings::item_count(lang, row.items.len()),
            row.size
        );
        let action = if row.restored {
            Tag::secondary()
                .xsmall()
                .child(ui(lang, Key::Restored))
                .into_any_element()
        } else {
            let restoring = self.restoring.as_deref() == Some(row.id.as_str());
            Button::new(SharedString::from(format!("restore-{}", row.id)))
                .small()
                .outline()
                .icon(Icon::new(IconName::Undo2))
                .label(ui(lang, Key::Restore))
                .loading(restoring)
                .disabled(self.restoring.is_some())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.restore(restore_id.clone(), window, cx)
                }))
                .into_any_element()
        };
        let header =
            h_flex()
                .gap_3()
                .px_4()
                .py_3()
                .child(
                    h_flex()
                        .id(SharedString::from(format!("history-{}", row.id)))
                        .flex_1()
                        .min_w_0()
                        .gap_3()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if !this.expanded.remove(&toggle_id) {
                                this.expanded.insert(toggle_id.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .small()
                            .text_color(muted),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(h_flex().gap_2().child(kind_tag(lang, row.kind)).child(
                                    div().font_semibold().truncate().child(row.title.clone()),
                                ))
                                .child(div().text_xs().text_color(muted).child(meta)),
                        ),
                )
                .child(action);
        let items = expanded.then(|| {
            v_flex().children(row.items.iter().map(|item| {
                h_flex()
                    .gap_3()
                    .pl(px(44.))
                    .pr_4()
                    .py_1p5()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .when(item.restored || item.pending, |this| this.text_color(muted))
                            .child(item.original.clone()),
                    )
                    .when(item.restored, |this| {
                        this.child(Tag::secondary().xsmall().child(ui(lang, Key::Restored)))
                    })
                    .when(item.pending, |this| {
                        this.child(Tag::warning().xsmall().child(ui(lang, Key::HistoryPending)))
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(84.))
                            .text_right()
                            .text_color(muted)
                            .child(item.size.clone()),
                    )
            }))
        });
        ui::card(cx).overflow_hidden().child(header).children(items)
    }
}

impl Render for HistoryPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let header = ui::page_header(
            IconName::ArchiveRestore,
            ui(lang, Key::NavHistory),
            ui(lang, Key::HistorySubtitle),
            Button::new("history-refresh")
                .small()
                .ghost()
                .icon(Icon::new(IconName::RefreshCw))
                .label(ui(lang, Key::Refresh))
                .disabled(self.loading)
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            cx,
        );
        let rows = self.rows.clone();
        v_flex()
            .size_full()
            .child(header)
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_6().pb_3().child(ui::banner(
                    Tone::Danger,
                    IconName::TriangleAlert,
                    error,
                    cx,
                )))
            })
            .map(|this| {
                if self.loading && rows.is_empty() {
                    this.child(
                        h_flex()
                            .px_6()
                            .gap_2()
                            .text_sm()
                            .text_color(ui::muted(cx))
                            .child(Spinner::new().small())
                            .child(ui(lang, Key::HistoryLoading)),
                    )
                } else if rows.is_empty() {
                    this.child(ui::empty_state(
                        IconName::ArchiveRestore,
                        ui(lang, Key::HistoryEmpty),
                        ui(lang, Key::HistoryEmptyBody),
                        cx,
                    ))
                } else {
                    this.child(
                        div()
                            .id("history-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(
                                v_flex().px_6().pb_6().gap_3().children(
                                    rows.iter().map(|row| self.render_row(row, lang, cx)),
                                ),
                            ),
                    )
                }
            })
    }
}
