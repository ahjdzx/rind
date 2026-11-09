//! 孤儿残留：App 已经删了、数据还在。按推测的主人分组，可展开；勾选后移到废纸篓。

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::orphans::{OrphanGroup, OrphanReport};
use rind_core::remove::Batch;
use rind_core::{RecordKind, Source};

use crate::model::{self, GroupCheck};
use crate::services::{Services, TrashOp};
use crate::store::{Kind, Scans};
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone, TrashHost, TrashPlan};

pub struct OrphansPage {
    services: Arc<Services>,
    scans: Entity<Scans>,
    selection: BTreeSet<PathBuf>,
    expanded: HashSet<String>,
    seen_epoch: u64,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl OrphansPage {
    pub fn new(services: Arc<Services>, scans: Entity<Scans>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&scans, |this, _, cx| this.sync(cx)),
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
        ];
        let mut page = Self {
            services,
            scans,
            selection: BTreeSet::new(),
            expanded: HashSet::new(),
            seen_epoch: 0,
            busy: false,
            _subscriptions: subscriptions,
        };
        page.sync(cx);
        page
    }

    fn report(&self, cx: &App) -> Option<Arc<OrphanReport>> {
        self.scans.read(cx).orphans.data.clone()
    }

    /// 有新结果就换成 Rind 的建议选择、展开前几组；只是剪枝就去掉已经不在的选择。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (epoch, report) = {
            let slot = &self.scans.read(cx).orphans;
            (slot.epoch, slot.data.clone())
        };
        if let Some(report) = report {
            if epoch != self.seen_epoch {
                self.seen_epoch = epoch;
                self.selection = report.suggested().into_iter().collect();
                self.expanded = report
                    .groups
                    .iter()
                    .take(3)
                    .map(|g| g.key.clone())
                    .collect();
            } else {
                model::retain_existing(
                    &mut self.selection,
                    report
                        .groups
                        .iter()
                        .flat_map(|g| g.items.iter())
                        .map(|m| m.path.as_path()),
                );
            }
        }
        cx.notify();
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.scan_orphans(false, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.cancel(Kind::Orphans, cx));
    }

    fn select_suggested(&mut self, cx: &mut Context<Self>) {
        if let Some(report) = self.report(cx) {
            self.selection = report.suggested().into_iter().collect();
            cx.notify();
        }
    }

    fn toggle_group(&mut self, key: &str, on: bool, cx: &mut Context<Self>) {
        let Some(report) = self.report(cx) else {
            return;
        };
        if let Some(group) = report.groups.iter().find(|g| g.key == key) {
            model::set_group(
                &mut self.selection,
                group
                    .items
                    .iter()
                    .map(|m| (m.path.as_path(), m.selectable())),
                on,
            );
        }
        cx.notify();
    }

    fn toggle_item(&mut self, path: PathBuf, selectable: bool, on: bool, cx: &mut Context<Self>) {
        model::set_one(&mut self.selection, &path, selectable, on);
        cx.notify();
    }

    fn toggle_expanded(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    fn picked(&self, report: &OrphanReport) -> model::Picked {
        model::picked(
            &self.selection,
            report
                .groups
                .iter()
                .flat_map(|g| g.items.iter())
                .map(|m| (m.path.as_path(), m.size)),
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
        // 只选了一组就用它的名字当标题，否则统称"孤儿残留"。
        let groups: Vec<&OrphanGroup> = report
            .groups
            .iter()
            .filter(|g| g.items.iter().any(|m| self.selection.contains(&m.path)))
            .collect();
        let title = match groups.as_slice() {
            [only] => only.display_name.clone(),
            _ => ui(lang, Key::NavOrphans).to_string(),
        };
        let plan = TrashPlan {
            op: TrashOp::Paths {
                batch: Batch {
                    kind: RecordKind::Orphans,
                    title,
                    bundle_id: None,
                    source: Source::App,
                },
                candidates: report.removables(),
            },
            selected: self.selection.iter().cloned().collect(),
            picked: self.picked(&report),
            extra: vec![strings::groups_involved(lang, groups.len()).into()],
        };
        ui::confirm_trash(plan, window, cx);
    }

    fn render_group(
        &self,
        gix: usize,
        group: &OrphanGroup,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = ui::muted(cx);
        let expanded = self.expanded.contains(&group.key);
        let check = model::group_check(
            &self.selection,
            group
                .items
                .iter()
                .map(|m| (m.path.as_path(), m.selectable())),
        );
        let any_selectable = group.items.iter().any(|m| m.selectable());
        let key = group.key.clone();
        let toggle_key = group.key.clone();
        let env = self.services.env().clone();
        let picked_in_group = group
            .items
            .iter()
            .filter(|m| self.selection.contains(&m.path))
            .count();

        let header = h_flex()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                Checkbox::new(SharedString::from(format!("og-{gix}")))
                    .checked(check == GroupCheck::All)
                    .disabled(!any_selectable)
                    .on_change(
                        cx.listener(move |this, on: &bool, _, cx| this.toggle_group(&key, *on, cx)),
                    ),
            )
            .child(
                h_flex()
                    .id(SharedString::from(format!("og-head-{gix}")))
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_expanded(toggle_key.clone(), cx)
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
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_semibold()
                                            .truncate()
                                            .child(group.display_name.clone()),
                                    )
                                    .when_some(group.confidence(), |this, c| {
                                        this.child(ui::confidence_tag(lang, c))
                                    }),
                            )
                            .child(div().text_xs().text_color(muted).truncate().child(format!(
                                "{} · {}",
                                group.key,
                                strings::picked_of(lang, picked_in_group, group.items.len())
                            ))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_semibold()
                            .child(ui::bytes(lang, group.known_size())),
                    ),
            );

        let items =
            expanded.then(|| {
                v_flex().children(group.items.iter().enumerate().map(|(ix, m)| {
                    let path = m.path.clone();
                    let selectable = m.selectable();
                    let hold_back = m
                        .hold_back
                        .as_ref()
                        .map(|h| rind_i18n::describe_hold_back(lang, h));
                    let shared = (!m.rivals.is_empty()).then(|| {
                        strings::shared_with(
                            lang,
                            &m.rivals.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
                        )
                    });
                    h_flex()
                        .gap_3()
                        .pl(px(44.))
                        .pr_4()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            Checkbox::new(SharedString::from(format!("oi-{gix}-{ix}")))
                                .checked(self.selection.contains(&m.path))
                                .disabled(!selectable)
                                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                                    this.toggle_item(path.clone(), selectable, *on, cx)
                                })),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis_middle()
                                        .when(!selectable, |this| this.text_color(muted))
                                        .child(env.display(&m.path)),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(
                                            Tag::secondary()
                                                .xsmall()
                                                .child(rind_i18n::describe_kind(lang, m.kind)),
                                        )
                                        .child(div().flex_1().min_w_0().truncate().child(
                                            rind_i18n::describe_evidence(lang, &m.evidence),
                                        )),
                                )
                                .when_some(hold_back, |this, reason| {
                                    this.child(
                                        h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(cx.theme().warning)
                                            .child(Icon::new(IconName::TriangleAlert).xsmall())
                                            .child(div().truncate().child(match &shared {
                                                Some(shared) => format!("{reason}（{shared}）"),
                                                None => reason,
                                            })),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(84.))
                                .text_sm()
                                .text_right()
                                .when(m.size.known().is_none(), |this| this.text_color(muted))
                                .child(rind_i18n::format_size(lang, m.size)),
                        )
                }))
            });

        ui::card(cx).overflow_hidden().child(header).children(items)
    }
}

impl TrashHost for OrphansPage {
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

impl Render for OrphansPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let (running, progress, report) = {
            let slot = &self.scans.read(cx).orphans;
            (
                slot.is_running(),
                slot.progress().cloned(),
                slot.data.clone(),
            )
        };
        let action = if running {
            Button::new("orphans-cancel")
                .small()
                .outline()
                .icon(Icon::new(IconName::X))
                .label(ui(lang, Key::Cancel))
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
        } else {
            Button::new("orphans-scan")
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
            IconName::Ghost,
            ui(lang, Key::NavOrphans),
            ui(lang, Key::OrphansSubtitle),
            action,
            cx,
        );
        let ledger = self
            .services
            .ledger_error()
            .map(|e| strings::ledger_unavailable(lang, e));

        let mut page = v_flex().size_full().child(header);
        if let Some(message) = ledger {
            page = page.child(div().px_6().pb_3().child(ui::banner(
                Tone::Danger,
                IconName::TriangleAlert,
                message,
                cx,
            )));
        }
        if running {
            page = page.child(div().px_6().pb_3().child(ui::card(cx).p_4().child(
                ui::progress_block("orphans-progress", lang, progress.as_ref(), cx),
            )));
        }
        if report.as_ref().is_some_and(|r| r.degraded) {
            page = page.child(div().px_6().pb_3().child(ui::banner(
                Tone::Warning,
                IconName::TriangleAlert,
                rind_i18n::describe_orphans_degraded(lang),
                cx,
            )));
        }
        let Some(report) = report else {
            if running {
                return page;
            }
            return page.child(
                ui::empty_state(
                    IconName::Ghost,
                    ui(lang, Key::OrphansIdleTitle),
                    ui(lang, Key::OrphansIdleBody),
                    cx,
                )
                .child(
                    Button::new("orphans-start")
                        .primary()
                        .icon(Icon::new(IconName::ScanSearch))
                        .label(ui(lang, Key::StartScan))
                        .on_click(cx.listener(|this, _, _, cx| this.scan(cx))),
                ),
            );
        };
        if report.groups.is_empty() {
            return page.child(ui::empty_state(
                IconName::CircleCheck,
                ui(lang, Key::OrphansNoneTitle),
                ui(lang, Key::OrphansNoneBody),
                cx,
            ));
        }

        let picked = self.picked(&report);
        let total_items: usize = report.groups.iter().map(|g| g.items.len()).sum();
        let toolbar = h_flex()
            .px_6()
            .pb_3()
            .gap_2()
            .child(
                Button::new("orphans-suggested")
                    .small()
                    .outline()
                    .icon(Icon::new(IconName::Sparkles))
                    .label(ui(lang, Key::SelectSuggested))
                    .on_click(cx.listener(|this, _, _, cx| this.select_suggested(cx))),
            )
            .child(
                Button::new("orphans-clear")
                    .small()
                    .ghost()
                    .label(ui(lang, Key::ClearSelection))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selection.clear();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child(strings::orphan_totals(
                        lang,
                        report.groups.len(),
                        total_items,
                        &ui::bytes(lang, model::orphan_total_bytes(&report)),
                    )),
            );
        let list = div()
            .id("orphans-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(
                v_flex().px_6().pb_4().gap_3().children(
                    report
                        .groups
                        .iter()
                        .enumerate()
                        .map(|(gix, g)| self.render_group(gix, g, lang, cx)),
                ),
            );
        let no_ledger = self.services.ledger_error().is_some();
        let footer = ui::footer_bar(cx)
            .child(ui::picked_summary(lang, picked, cx))
            .child(
                Button::new("orphans-trash")
                    .danger()
                    .icon(Icon::new(IconName::Trash))
                    .label(ui(lang, Key::MoveToTrash))
                    .loading(self.busy)
                    .disabled(picked.count == 0 || self.busy || running || no_ledger)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
            );
        page.child(toolbar).child(list).child(footer)
    }
}
