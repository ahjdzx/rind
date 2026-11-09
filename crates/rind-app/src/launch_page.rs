//! 启动项：LaunchAgents / LaunchDaemons。看清谁在开机时跑、哪些已经没了主人；
//! 当前用户的可以停用/启用、移除，其余的说明为什么不能动。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::launch::{LaunchItem, LaunchStatus};

use crate::model::{self, LaunchFilter, Picked};
use crate::services::{Services, TrashOp};
use crate::store::Scans;
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone, TrashHost, TrashPlan};

pub struct LaunchPage {
    services: Arc<Services>,
    scans: Entity<Scans>,
    filter: LaunchFilter,
    /// 正在停用/启用/移除的那一项（plist 路径）。
    busy: Option<std::path::PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl LaunchPage {
    pub fn new(services: Arc<Services>, scans: Entity<Scans>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&scans, |_, _, cx| cx.notify()),
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
        ];
        Self {
            services,
            scans,
            filter: LaunchFilter::All,
            busy: None,
            _subscriptions: subscriptions,
        }
    }

    /// 第一次进这一页还没有数据就读一次（很快，不用等智能扫描）。
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        let needs = {
            let slot = &self.scans.read(cx).launch;
            slot.data.is_none() && !slot.is_running()
        };
        if needs {
            self.reload(cx);
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.load_launch(false, cx));
    }

    fn set_enabled(
        &mut self,
        item: LaunchItem,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(item.plist.clone());
        cx.notify();
        let services = self.services.clone();
        cx.spawn_in(window, async move |this, cx| {
            let task_item = item.clone();
            let result = cx
                .background_spawn(async move { services.set_launch_enabled(&task_item, enabled) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                let lang = strings::lang(cx);
                let name = model::launch_name(&item);
                match result {
                    Ok(()) => {
                        let status = if enabled {
                            LaunchStatus::Loaded { pid: None }
                        } else {
                            LaunchStatus::Disabled
                        };
                        this.scans
                            .update(cx, |s, cx| s.set_launch_status(&item.plist, status, cx));
                        window.push_notification(
                            Notification::success(strings::launch_toggled(lang, &name, enabled)),
                            cx,
                        );
                        // 真实模式再读一次，拿到 launchctl 的真实状态。
                        if !this.services.is_demo() {
                            this.reload(cx);
                        }
                    }
                    Err(error) => window.push_notification(
                        Notification::error(strings::launch_toggle_failed(
                            lang, &name, enabled, &error,
                        ))
                        .title(ui(lang, Key::NothingChanged)),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_remove(&mut self, item: LaunchItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(items) = self.scans.read(cx).launch.data.clone() else {
            return;
        };
        let lang = strings::lang(cx);
        let plan = TrashPlan {
            op: TrashOp::Launch {
                items: items.as_ref().clone(),
            },
            selected: [item.plist.clone()].into(),
            picked: Picked {
                count: 1,
                bytes: 0,
                unknown: 0,
            },
            extra: vec![strings::launch_remove_note(lang, &model::launch_name(&item)).into()],
        };
        ui::confirm_trash(plan, window, cx);
    }

    fn render_item(&self, ix: usize, item: &LaunchItem, lang: Lang, cx: &mut Context<Self>) -> Div {
        let muted = ui::muted(cx);
        let danger = cx.theme().danger;
        let name = model::launch_name(item);
        let busy = self.busy.as_ref() == Some(&item.plist);
        let any_busy = self.busy.is_some();
        let running = matches!(item.status, LaunchStatus::Loaded { pid: Some(_) });
        let status_color = match item.status {
            LaunchStatus::Loaded { pid: Some(_) } => cx.theme().success,
            LaunchStatus::Disabled => cx.theme().warning,
            _ => muted,
        };
        let owner = item
            .owner
            .as_ref()
            .map(|o| strings::launch_owner(lang, &o.name))
            .unwrap_or_else(|| ui(lang, Key::LaunchNoOwner).to_string());
        let program = item
            .program
            .as_ref()
            .map(|p| self.services.env().display(p))
            .unwrap_or_else(|| ui(lang, Key::LaunchNoProgram).to_string());

        let actions = if item.manageable() {
            let disabled = matches!(item.status, LaunchStatus::Disabled);
            let toggle_item = item.clone();
            let remove_item = item.clone();
            h_flex()
                .gap_1()
                .child(
                    Button::new(("launch-toggle", ix))
                        .small()
                        .outline()
                        .icon(Icon::new(if disabled {
                            IconName::Play
                        } else {
                            IconName::Pause
                        }))
                        .label(if disabled {
                            ui(lang, Key::LaunchEnable)
                        } else {
                            ui(lang, Key::LaunchDisable)
                        })
                        .loading(busy)
                        .disabled(any_busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_enabled(toggle_item.clone(), disabled, window, cx)
                        })),
                )
                .child(
                    Button::new(("launch-remove", ix))
                        .small()
                        .ghost()
                        .icon(Icon::new(IconName::Trash).text_color(danger))
                        .tooltip(ui(lang, Key::LaunchRemove))
                        .disabled(any_busy || self.services.ledger_error().is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove(remove_item.clone(), window, cx)
                        })),
                )
                .into_any_element()
        } else {
            let why = match &item.hold_back {
                Some(h) => rind_i18n::describe_hold_back(lang, h),
                None => ui(lang, Key::LaunchNoLabel).to_string(),
            };
            h_flex()
                .max_w(px(220.))
                .gap_1()
                .text_xs()
                .text_color(muted)
                .child(Icon::new(IconName::Lock).xsmall())
                .child(div().truncate().child(why))
                .into_any_element()
        };

        h_flex()
            .gap_3()
            .px_4()
            .py_3()
            .when(ix > 0, |this| {
                this.border_t_1().border_color(cx.theme().border)
            })
            .child(ui::icon_tile(
                IconName::Rocket,
                if item.orphan {
                    Tone::Danger
                } else {
                    Tone::Neutral
                },
                px(32.),
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_semibold().truncate().child(name))
                            .when(item.orphan, |this| {
                                this.child(
                                    Tag::danger().xsmall().child(ui(lang, Key::LaunchOrphan)),
                                )
                            })
                            .child(
                                Tag::secondary()
                                    .xsmall()
                                    .child(rind_i18n::describe_launch_domain(lang, item.domain)),
                            ),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(owner))
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(if item.program_exists { muted } else { danger })
                            .when(!item.program_exists, |this| {
                                this.child(Icon::new(IconName::CircleAlert).xsmall())
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .child(if item.program_exists {
                                        program
                                    } else {
                                        strings::program_missing(lang, &program)
                                    }),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .w(px(120.))
                    .gap_1p5()
                    .text_xs()
                    .text_color(status_color)
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(status_color)
                            .when(!running, |d| d.opacity(0.6)),
                    )
                    .child(rind_i18n::describe_launch_status(lang, item.status)),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_end()
                    .w(px(220.))
                    .child(actions),
            )
    }
}

impl TrashHost for LaunchPage {
    fn services(&self) -> Arc<Services> {
        self.services.clone()
    }
    fn scans(&self) -> Entity<Scans> {
        self.scans.clone()
    }
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy.then(std::path::PathBuf::new);
    }
}

impl Render for LaunchPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let (loading, items) = {
            let slot = &self.scans.read(cx).launch;
            (slot.is_running(), slot.data.clone())
        };
        let header = ui::page_header(
            IconName::Rocket,
            ui(lang, Key::NavLaunch),
            ui(lang, Key::LaunchSubtitle),
            Button::new("launch-refresh")
                .small()
                .outline()
                .icon(Icon::new(IconName::RefreshCw))
                .label(ui(lang, Key::Refresh))
                .loading(loading)
                .disabled(loading || self.busy.is_some())
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            cx,
        );
        let page = v_flex().size_full().child(header);
        let Some(items) = items else {
            return page.child(
                h_flex()
                    .px_6()
                    .gap_2()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child(Spinner::new().small())
                    .child(ui(lang, Key::LaunchLoading)),
            );
        };
        let filters = LaunchFilter::ALL;
        let selected = filters.iter().position(|f| *f == self.filter).unwrap_or(0);
        let tabs = TabBar::new("launch-filter")
            .segmented()
            .selected_index(selected)
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                this.filter = filters.get(*ix).copied().unwrap_or(LaunchFilter::All);
                cx.notify();
            }))
            .children(filters.iter().map(|f| {
                let count = items.iter().filter(|i| f.matches(i)).count();
                let label = match f {
                    LaunchFilter::All => ui(lang, Key::FilterAll),
                    LaunchFilter::Orphans => ui(lang, Key::FilterOrphans),
                    LaunchFilter::User => ui(lang, Key::FilterUser),
                };
                Tab::new().label(format!("{label} {count}"))
            }));
        let visible: Vec<(usize, &LaunchItem)> = items
            .iter()
            .filter(|i| self.filter.matches(i))
            .enumerate()
            .collect();
        let orphans = items.iter().filter(|i| i.orphan).count();
        let toolbar = h_flex()
            .px_6()
            .pb_3()
            .gap_3()
            .child(tabs)
            .child(div().flex_1())
            .when(orphans > 0, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(strings::launch_orphans_hint(lang, orphans)),
                )
            });
        let body = if visible.is_empty() {
            ui::empty_state(
                IconName::CircleCheck,
                ui(lang, Key::LaunchEmpty),
                ui(lang, Key::LaunchEmptyBody),
                cx,
            )
            .into_any_element()
        } else {
            div()
                .id("launch-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(
                    div().px_6().pb_6().child(
                        ui::card(cx).overflow_hidden().children(
                            visible
                                .into_iter()
                                .map(|(ix, item)| self.render_item(ix, item, lang, cx)),
                        ),
                    ),
                )
                .into_any_element()
        };
        page.when_some(
            self.services
                .ledger_error()
                .map(|e| strings::ledger_unavailable(lang, e)),
            |this, message| {
                this.child(div().px_6().pb_3().child(ui::banner(
                    Tone::Danger,
                    IconName::TriangleAlert,
                    message,
                    cx,
                )))
            },
        )
        .child(toolbar)
        .child(body)
    }
}
