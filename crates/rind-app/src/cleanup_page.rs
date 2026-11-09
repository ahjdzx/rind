//! 缓存清理：按类别分节，每节有说明、合计和整节开关；底部汇总已选多少、能释放多少。

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::cleanup::{CleanupCategory, CleanupItem, CleanupReport};
use rind_core::remove::Batch;
use rind_core::{HoldBack, RecordKind, Source};

use crate::model::{self, CleanupSection, GroupCheck};
use crate::services::{CleanupPrefs, Services, TrashOp};
use crate::store::{self, Kind, Scans};
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone, TrashHost, TrashPlan};

/// 一节超过这么多项时默认收起。
const COLLAPSE_OVER: usize = 12;

pub struct CleanupPage {
    services: Arc<Services>,
    scans: Entity<Scans>,
    selection: BTreeSet<PathBuf>,
    collapsed: HashSet<CleanupCategory>,
    seen_epoch: u64,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl CleanupPage {
    pub fn new(services: Arc<Services>, scans: Entity<Scans>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&scans, |this, _, cx| this.sync(cx)),
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
            cx.observe_global::<CleanupPrefs>(|_, cx| cx.notify()),
        ];
        let mut page = Self {
            services,
            scans,
            selection: BTreeSet::new(),
            collapsed: HashSet::new(),
            seen_epoch: 0,
            busy: false,
            _subscriptions: subscriptions,
        };
        page.sync(cx);
        page
    }

    fn report(&self, cx: &App) -> Option<Arc<CleanupReport>> {
        self.scans.read(cx).cleanup.data.clone()
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (epoch, report) = {
            let slot = &self.scans.read(cx).cleanup;
            (slot.epoch, slot.data.clone())
        };
        if let Some(report) = report {
            if epoch != self.seen_epoch {
                self.seen_epoch = epoch;
                self.selection = report.suggested().into_iter().collect();
                self.collapsed = model::cleanup_sections(&report)
                    .into_iter()
                    .filter(|s| s.items.len() > COLLAPSE_OVER)
                    .map(|s| s.category)
                    .collect();
            } else {
                model::retain_existing(
                    &mut self.selection,
                    report.items.iter().map(|i| i.path.as_path()),
                );
            }
        }
        cx.notify();
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let prefs = store::cleanup_prefs(cx);
        self.scans
            .update(cx, |s, cx| s.scan_cleanup(prefs, false, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.cancel(Kind::Cleanup, cx));
    }

    fn toggle_section(&mut self, category: CleanupCategory, on: bool, cx: &mut Context<Self>) {
        let Some(report) = self.report(cx) else {
            return;
        };
        model::set_group(
            &mut self.selection,
            report
                .items
                .iter()
                .filter(|i| i.category == category)
                .map(|i| (i.path.as_path(), i.selectable())),
            on,
        );
        cx.notify();
    }

    fn toggle_collapsed(&mut self, category: CleanupCategory, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&category) {
            self.collapsed.insert(category);
        }
        cx.notify();
    }

    fn picked(&self, report: &CleanupReport) -> model::Picked {
        model::picked(
            &self.selection,
            report.items.iter().map(|i| (i.path.as_path(), i.size)),
        )
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(report) = self.report(cx) else {
            return;
        };
        if self.busy || self.selection.is_empty() {
            return;
        }
        let lang = strings::lang(cx);
        let plan = TrashPlan {
            op: TrashOp::Cleanup {
                batch: Batch {
                    kind: RecordKind::Cleanup,
                    title: ui(lang, Key::NavCleanup).to_string(),
                    bundle_id: None,
                    source: Source::App,
                },
                report: (*report).clone(),
            },
            selected: self.selection.iter().cloned().collect(),
            picked: self.picked(&report),
            extra: Vec::new(),
        };
        ui::confirm_trash(plan, window, cx);
    }

    fn render_item(
        &self,
        ix: usize,
        item: &CleanupItem,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = ui::muted(cx);
        let selectable = item.selectable();
        let path = item.path.clone();
        let hold_back: Option<(Tone, String)> = item.hold_back.as_ref().map(|h| match h {
            HoldBack::InUse { app } => (Tone::Warning, strings::in_use_rescan(lang, app)),
            other => (Tone::Neutral, rind_i18n::describe_hold_back(lang, other)),
        });
        h_flex()
            .gap_3()
            .pl(px(44.))
            .pr_4()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Checkbox::new(SharedString::from(format!("ci-{ix}")))
                    .checked(self.selection.contains(&item.path))
                    .disabled(!selectable)
                    .on_change(cx.listener(move |this, on: &bool, _, cx| {
                        model::set_one(&mut this.selection, &path, selectable, *on);
                        cx.notify();
                    })),
            )
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
                                    .text_sm()
                                    .truncate()
                                    .when(!selectable, |this| this.text_color(muted))
                                    .child(item.label.clone()),
                            )
                            .when(item.suggested, |this| {
                                this.child(
                                    Icon::new(IconName::Sparkles)
                                        .xsmall()
                                        .text_color(cx.theme().primary),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .child(self.services.env().display(&item.path)),
                    )
                    .when_some(hold_back, |this, (tone, message)| {
                        let color = ui::tone_color(tone, cx);
                        this.child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(color)
                                .when(tone == Tone::Warning, |this| this.font_semibold())
                                .child(Icon::new(IconName::TriangleAlert).xsmall())
                                .child(div().truncate().child(message)),
                        )
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(96.))
                    .text_xs()
                    .text_color(muted)
                    .text_right()
                    .child(
                        item.modified
                            .map(|m| strings::relative_time(lang, chrono::Utc::now(), m))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(84.))
                    .text_sm()
                    .text_right()
                    .when(item.size.known().is_none(), |this| this.text_color(muted))
                    .child(rind_i18n::format_size(lang, item.size)),
            )
    }

    fn render_section(
        &self,
        section: &CleanupSection,
        report: &CleanupReport,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = ui::muted(cx);
        let category = section.category;
        let collapsed = self.collapsed.contains(&category);
        let items: Vec<&CleanupItem> = section.items.iter().map(|&ix| &report.items[ix]).collect();
        let check = model::group_check(
            &self.selection,
            items.iter().map(|i| (i.path.as_path(), i.selectable())),
        );
        let any_selectable = items.iter().any(|i| i.selectable());
        let picked = model::picked(
            &self.selection,
            items.iter().map(|i| (i.path.as_path(), i.size)),
        );

        let header = h_flex()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                h_flex()
                    .id(SharedString::from(format!("cs-{category:?}")))
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_collapsed(category, cx)
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .small()
                        .text_color(muted),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().font_semibold().child(
                                        rind_i18n::describe_cleanup_category(lang, category),
                                    ))
                                    .child(div().text_xs().text_color(muted).child(
                                        strings::picked_of(lang, picked.count, items.len()),
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(rind_i18n::explain_cleanup_category(lang, category)),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_semibold()
                            .child(ui::bytes(lang, section.total)),
                    ),
            )
            .child(
                Switch::new(SharedString::from(format!("cs-switch-{category:?}")))
                    .checked(check == GroupCheck::All)
                    .disabled(!any_selectable)
                    .on_change(cx.listener(move |this, on: &bool, _, cx| {
                        this.toggle_section(category, *on, cx)
                    })),
            );
        let body = (!collapsed).then(|| {
            v_flex().children(
                section
                    .items
                    .iter()
                    .map(|&ix| self.render_item(ix, &report.items[ix], lang, cx)),
            )
        });
        ui::card(cx).overflow_hidden().child(header).children(body)
    }
}

impl TrashHost for CleanupPage {
    fn services(&self) -> Arc<Services> {
        self.services.clone()
    }
    fn scans(&self) -> Entity<Scans> {
        self.scans.clone()
    }
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }
}

impl Render for CleanupPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let (running, progress, report) = {
            let slot = &self.scans.read(cx).cleanup;
            (
                slot.is_running(),
                slot.progress().cloned(),
                slot.data.clone(),
            )
        };
        let action = if running {
            Button::new("cleanup-cancel")
                .small()
                .outline()
                .icon(Icon::new(IconName::X))
                .label(ui(lang, Key::Cancel))
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
        } else {
            Button::new("cleanup-scan")
                .small()
                .outline()
                .icon(Icon::new(IconName::RefreshCw))
                .label(if report.is_some() {
                    ui(lang, Key::Rescan)
                } else {
                    ui(lang, Key::StartScan)
                })
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| this.scan(cx)))
        };
        let header = ui::page_header(
            IconName::Brush,
            ui(lang, Key::NavCleanup),
            ui(lang, Key::CleanupSubtitle),
            action,
            cx,
        );
        let prefs = store::cleanup_prefs(cx);
        let roots = if prefs.roots.is_empty() {
            self.services.default_project_roots()
        } else {
            prefs.roots.clone()
        };
        let roots: Vec<String> = roots
            .iter()
            .map(|r| self.services.env().display(r))
            .collect();
        let prefs_line = div()
            .px_6()
            .pb_3()
            .text_xs()
            .text_color(ui::muted(cx))
            .child(strings::cleanup_prefs_line(
                lang,
                &roots,
                prefs.min_age_days,
            ));

        let mut page = v_flex().size_full().child(header).child(prefs_line);
        if let Some(message) = self
            .services
            .ledger_error()
            .map(|e| strings::ledger_unavailable(lang, e))
        {
            page = page.child(div().px_6().pb_3().child(ui::banner(
                Tone::Danger,
                IconName::TriangleAlert,
                message,
                cx,
            )));
        }
        if running {
            page = page.child(div().px_6().pb_3().child(ui::card(cx).p_4().child(
                ui::progress_block("cleanup-progress", lang, progress.as_ref(), cx),
            )));
        }
        let Some(report) = report else {
            if running {
                return page;
            }
            return page.child(
                ui::empty_state(
                    IconName::Brush,
                    ui(lang, Key::CleanupIdleTitle),
                    ui(lang, Key::CleanupIdleBody),
                    cx,
                )
                .child(
                    Button::new("cleanup-start")
                        .primary()
                        .icon(Icon::new(IconName::ScanSearch))
                        .label(ui(lang, Key::StartScan))
                        .on_click(cx.listener(|this, _, _, cx| this.scan(cx))),
                ),
            );
        };
        let sections = model::cleanup_sections(&report);
        if sections.is_empty() {
            return page.child(ui::empty_state(
                IconName::CircleCheck,
                ui(lang, Key::CleanupNoneTitle),
                ui(lang, Key::CleanupNoneBody),
                cx,
            ));
        }
        let picked = self.picked(&report);
        let in_use = report
            .items
            .iter()
            .filter(|i| matches!(i.hold_back, Some(HoldBack::InUse { .. })))
            .count();
        let toolbar = h_flex()
            .px_6()
            .pb_3()
            .gap_2()
            .child(
                Button::new("cleanup-suggested")
                    .small()
                    .outline()
                    .icon(Icon::new(IconName::Sparkles))
                    .label(ui(lang, Key::SelectSuggested))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(report) = this.report(cx) {
                            this.selection = report.suggested().into_iter().collect();
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("cleanup-clear")
                    .small()
                    .ghost()
                    .label(ui(lang, Key::ClearSelection))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selection.clear();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .when(in_use > 0, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(IconName::TriangleAlert).small())
                        .child(strings::in_use_count(lang, in_use)),
                )
            });
        let list = div()
            .id("cleanup-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(
                v_flex().px_6().pb_4().gap_3().children(
                    sections
                        .iter()
                        .map(|s| self.render_section(s, &report, lang, cx)),
                ),
            );
        let no_ledger = self.services.ledger_error().is_some();
        let footer = ui::footer_bar(cx)
            .child(ui::picked_summary(lang, picked, cx))
            .child(
                Button::new("cleanup-trash")
                    .danger()
                    .icon(Icon::new(IconName::Trash))
                    .label(ui(lang, Key::CleanSelected))
                    .loading(self.busy)
                    .disabled(picked.count == 0 || self.busy || running || no_ledger)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
            );
        page.child(toolbar).child(list).child(footer)
    }
}
