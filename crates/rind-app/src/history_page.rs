//! 历史页：Rind 移走过什么，一键放回。

use std::sync::Arc;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::RestoreOutcome;

use crate::services::{HistoryRow, Services};
use crate::strings::{self, ui, Key, LangSetting};

pub struct HistoryPage {
    services: Arc<Services>,
    rows: Vec<HistoryRow>,
    loading: bool,
    error: Option<SharedString>,
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

    fn render_row(&self, row: &HistoryRow, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let id = row.id.clone();
        let meta = format!(
            "{}  ·  {}  ·  {}",
            row.when,
            strings::item_count(lang, row.items),
            row.size
        );
        h_flex()
            .gap_3()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_semibold().truncate().child(row.app_name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(meta),
                    ),
            )
            .map(|this| {
                if row.restored {
                    this.child(Tag::secondary().xsmall().child(ui(lang, Key::Restored)))
                } else {
                    let restoring = self.restoring.as_deref() == Some(row.id.as_str());
                    this.child(
                        Button::new(ElementId::from((
                            ElementId::from("restore"),
                            row.id.clone(),
                        )))
                        .small()
                        .outline()
                        .icon(Icon::new(IconName::Undo2))
                        .label(ui(lang, Key::Restore))
                        .loading(restoring)
                        .disabled(self.restoring.is_some())
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.restore(id.clone(), window, cx),
                        )),
                    )
                }
            })
    }
}

impl Render for HistoryPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let muted = cx.theme().muted_foreground;
        let rows: Vec<HistoryRow> = self.rows.clone();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(
                div()
                    .text_xl()
                    .font_bold()
                    .child(ui(lang, Key::HistoryTitle)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .map(|this| {
                if self.loading && rows.is_empty() {
                    this.child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .text_color(muted)
                            .child(Spinner::new().small())
                            .child(ui(lang, Key::HistoryLoading)),
                    )
                } else if rows.is_empty() {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(ui(lang, Key::HistoryEmpty)),
                    )
                } else {
                    this.child(
                        div()
                            .id("history-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(
                                v_flex()
                                    .gap_2()
                                    .pr_2()
                                    .children(rows.iter().map(|row| self.render_row(row, cx))),
                            ),
                    )
                }
            })
    }
}
