//! 总览：磁盘容量、智能扫描、五张功能卡片。卡片的数字都来自共享仓库，点卡片跳到对应页。

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::*;

use crate::model::{self, Page};
use crate::store::{self, Scans, Slot};
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone};

/// 点了某张卡片：去那一页。
pub struct NavigateTo(pub Page);

pub struct OverviewPage {
    scans: Entity<Scans>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<NavigateTo> for OverviewPage {}

/// 卡片上的数字现在是什么状态。
enum Figure {
    Loading(String),
    Ready { value: String, detail: String },
    Empty(String),
}

impl OverviewPage {
    pub fn new(scans: Entity<Scans>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&scans, |_, _, cx| cx.notify()),
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
        ];
        Self {
            scans,
            _subscriptions: subscriptions,
        }
    }

    fn smart_scan(&mut self, cx: &mut Context<Self>) {
        let prefs = store::cleanup_prefs(cx);
        self.scans.update(cx, |s, cx| s.smart_scan(prefs, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.cancel_smart(cx));
    }

    fn render_disk(&self, lang: Lang, cx: &App) -> Div {
        let scans = self.scans.read(cx);
        let muted = ui::muted(cx);
        let body = match scans.disk {
            Some(disk) if disk.total > 0 => {
                let fraction = disk.used() as f32 / disk.total as f32;
                let tone = if fraction > 0.9 {
                    Tone::Danger
                } else if fraction > 0.75 {
                    Tone::Warning
                } else {
                    Tone::Primary
                };
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(ui::icon_tile(
                                IconName::HardDrive,
                                Tone::Primary,
                                px(36.),
                                cx,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_0p5()
                                    .child(div().font_semibold().child(ui(lang, Key::DiskTitle)))
                                    .child(div().text_sm().text_color(muted).child(
                                        strings::disk_line(
                                            lang,
                                            &ui::bytes(lang, disk.used()),
                                            &ui::bytes(lang, disk.available),
                                            &ui::bytes(lang, disk.total),
                                        ),
                                    )),
                            )
                            .child(
                                div()
                                    .text_2xl()
                                    .font_semibold()
                                    .text_color(ui::tone_color(tone, cx))
                                    .child(format!("{:.0}%", fraction * 100.0)),
                            ),
                    )
                    .child(ui::bar(fraction, ui::tone_color(tone, cx), cx).h(px(10.)))
                    .child(
                        h_flex()
                            .gap_4()
                            .text_xs()
                            .text_color(muted)
                            .child(legend(ui::tone_color(tone, cx), ui(lang, Key::DiskUsed)))
                            .child(legend(cx.theme().muted, ui(lang, Key::DiskFree))),
                    )
            }
            _ => h_flex()
                .gap_3()
                .child(ui::icon_tile(
                    IconName::HardDrive,
                    Tone::Neutral,
                    px(36.),
                    cx,
                ))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(ui(lang, Key::DiskUnknown)),
                ),
        };
        ui::card(cx).p_5().child(body)
    }

    fn render_hero(&self, lang: Lang, cx: &mut Context<Self>) -> Div {
        let scans = self.scans.read(cx);
        let running = scans.smart_running();
        let has_results = scans.orphans.data.is_some() || scans.cleanup.data.is_some();
        let primary = cx.theme().primary;
        let reclaim = scans
            .orphans
            .data
            .as_ref()
            .map_or(0, |r| model::orphan_suggested_bytes(r))
            + scans
                .cleanup
                .data
                .as_ref()
                .map_or(0, |r| model::cleanup_suggested_bytes(r));

        let (title, body): (String, String) = if running {
            (
                ui(lang, Key::SmartRunning).into(),
                ui(lang, Key::SmartRunningBody).into(),
            )
        } else if has_results {
            (
                strings::reclaimable(lang, &ui::bytes(lang, reclaim)),
                ui(lang, Key::SmartDoneBody).into(),
            )
        } else {
            (
                ui(lang, Key::SmartTitle).into(),
                ui(lang, Key::SmartBody).into(),
            )
        };

        let action = if running {
            Button::new("smart-cancel")
                .outline()
                .icon(Icon::new(IconName::X))
                .label(ui(lang, Key::Cancel))
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
        } else {
            Button::new("smart-scan")
                .primary()
                .large()
                .icon(Icon::new(IconName::Sparkles))
                .label(if has_results {
                    ui(lang, Key::SmartAgain)
                } else {
                    ui(lang, Key::SmartStart)
                })
                .on_click(cx.listener(|this, _, _, cx| this.smart_scan(cx)))
        };

        let tasks = running.then(|| {
            v_flex()
                .gap_2()
                .pt_2()
                .child(task_row(
                    lang,
                    ui(lang, Key::NavOrphans),
                    &scans.orphans,
                    cx,
                ))
                .child(task_row(
                    lang,
                    ui(lang, Key::NavCleanup),
                    &scans.cleanup,
                    cx,
                ))
                .child(task_row(lang, ui(lang, Key::NavLaunch), &scans.launch, cx))
        });

        ui::card(cx)
            .p_5()
            .gap_3()
            .bg(primary.opacity(0.06))
            .border_color(primary.opacity(0.25))
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .size(px(48.))
                            .justify_center()
                            .rounded_full()
                            .bg(primary.opacity(0.14))
                            .text_color(primary)
                            .child(if running {
                                Spinner::new().into_any_element()
                            } else {
                                Icon::new(IconName::Sparkles).into_any_element()
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_lg().font_semibold().child(title))
                            .child(div().text_sm().text_color(ui::muted(cx)).child(body)),
                    )
                    .child(action),
            )
            .children(tasks)
    }

    fn figures(&self, page: Page, lang: Lang, cx: &App) -> Figure {
        let scans = self.scans.read(cx);
        let loading = |slot_running: bool, progress: Option<&rind_core::ScanProgress>| {
            slot_running.then(|| Figure::Loading(ui::scan_message(lang, progress)))
        };
        let not_scanned = || Figure::Empty(ui(lang, Key::CardNotScanned).into());
        match page {
            Page::Apps => {
                if let Some(apps) = &scans.apps.data {
                    let total: u64 = apps.iter().filter_map(|a| a.app.size.known()).sum();
                    Figure::Ready {
                        value: strings::count(lang, apps.len()),
                        detail: strings::apps_total(lang, &ui::bytes(lang, total)),
                    }
                } else {
                    Figure::Loading(ui(lang, Key::AppsLoading).into())
                }
            }
            Page::Orphans => loading(scans.orphans.is_running(), scans.orphans.progress())
                .or_else(|| {
                    scans.orphans.data.as_ref().map(|r| Figure::Ready {
                        value: ui::bytes(lang, model::orphan_total_bytes(r)),
                        detail: strings::orphan_card(
                            lang,
                            r.groups.len(),
                            &ui::bytes(lang, model::orphan_suggested_bytes(r)),
                        ),
                    })
                })
                .unwrap_or_else(not_scanned),
            Page::Cleanup => loading(scans.cleanup.is_running(), scans.cleanup.progress())
                .or_else(|| {
                    scans.cleanup.data.as_ref().map(|r| {
                        let total: u64 = r.items.iter().filter_map(|i| i.size.known()).sum();
                        Figure::Ready {
                            value: ui::bytes(lang, model::cleanup_suggested_bytes(r)),
                            detail: strings::cleanup_card(
                                lang,
                                r.items.len(),
                                &ui::bytes(lang, total),
                            ),
                        }
                    })
                })
                .unwrap_or_else(not_scanned),
            Page::Space => loading(scans.space.is_running(), scans.space.progress())
                .or_else(|| {
                    scans.space.data.as_ref().map(|r| {
                        let total: u64 = r.large_files.iter().map(|f| f.size).sum();
                        Figure::Ready {
                            value: strings::count(lang, r.large_files.len()),
                            detail: strings::large_files_card(
                                lang,
                                &ui::bytes(lang, total),
                                &scans.services().env().display(&r.root.path),
                            ),
                        }
                    })
                })
                .unwrap_or_else(|| Figure::Empty(ui(lang, Key::CardSpaceHint).into())),
            Page::Launch => loading(scans.launch.is_running(), None)
                .or_else(|| {
                    scans.launch.data.as_ref().map(|items| {
                        let orphans = items.iter().filter(|i| i.orphan).count();
                        let running = items
                            .iter()
                            .filter(|i| {
                                matches!(
                                    i.status,
                                    rind_core::launch::LaunchStatus::Loaded { pid: Some(_) }
                                )
                            })
                            .count();
                        Figure::Ready {
                            value: strings::count(lang, items.len()),
                            detail: strings::launch_card(lang, orphans, running),
                        }
                    })
                })
                .unwrap_or_else(not_scanned),
            Page::Overview | Page::History | Page::Settings => not_scanned(),
        }
    }

    fn render_card(
        &self,
        page: Page,
        icon: IconName,
        tone: Tone,
        title: &'static str,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let figure = self.figures(page, lang, cx);
        let muted = ui::muted(cx);
        let hover = cx.theme().secondary;
        let body = match figure {
            Figure::Loading(message) => v_flex()
                .gap_1()
                .child(h_flex().h(px(32.)).child(Spinner::new().small()))
                .child(div().text_xs().text_color(muted).truncate().child(message)),
            Figure::Ready { value, detail } => v_flex()
                .gap_1()
                .child(div().text_2xl().font_semibold().child(value))
                .child(div().text_xs().text_color(muted).truncate().child(detail)),
            Figure::Empty(message) => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_2xl()
                        .font_semibold()
                        .text_color(muted)
                        .child("—"),
                )
                .child(div().text_xs().text_color(muted).truncate().child(message)),
        };
        ui::card(cx)
            .id(SharedString::from(format!("card-{page:?}")))
            .flex_1()
            .min_w(px(220.))
            .p_4()
            .gap_3()
            .cursor_pointer()
            .hover(move |this| this.bg(hover))
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(NavigateTo(page))))
            .child(
                h_flex()
                    .gap_2()
                    .child(ui::icon_tile(icon, tone, px(28.), cx))
                    .child(div().flex_1().text_sm().font_semibold().child(title))
                    .child(Icon::new(IconName::ChevronRight).small().text_color(muted)),
            )
            .child(body)
    }
}

fn legend(color: Hsla, label: &'static str) -> Div {
    h_flex()
        .gap_1p5()
        .child(div().size(px(8.)).rounded_full().bg(color))
        .child(label)
}

/// 智能扫描里的一项：在跑就转圈并说进度，跑完打勾。
fn task_row<T>(lang: Lang, label: &'static str, slot: &Slot<T>, cx: &App) -> Div {
    let muted = ui::muted(cx);
    let (icon, message): (AnyElement, String) = if slot.is_running() {
        (
            Spinner::new().small().into_any_element(),
            ui::scan_message(lang, slot.progress()),
        )
    } else if slot.data.is_some() {
        (
            Icon::new(IconName::CircleCheck)
                .small()
                .text_color(cx.theme().success)
                .into_any_element(),
            ui(lang, Key::TaskDone).into(),
        )
    } else {
        (
            Icon::new(IconName::Clock)
                .small()
                .text_color(muted)
                .into_any_element(),
            ui(lang, Key::TaskWaiting).into(),
        )
    };
    h_flex()
        .gap_2()
        .text_sm()
        .child(icon)
        .child(div().w(px(88.)).font_semibold().child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(muted)
                .child(message),
        )
}

impl Render for OverviewPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let header = ui::page_header(
            IconName::LayoutDashboard,
            ui(lang, Key::NavOverview),
            ui(lang, Key::OverviewSubtitle),
            div(),
            cx,
        );
        let cards_top = h_flex()
            .gap_4()
            .flex_wrap()
            .child(self.render_card(
                Page::Apps,
                IconName::AppWindow,
                Tone::Primary,
                ui(lang, Key::NavApps),
                lang,
                cx,
            ))
            .child(self.render_card(
                Page::Orphans,
                IconName::Ghost,
                Tone::Warning,
                ui(lang, Key::NavOrphans),
                lang,
                cx,
            ))
            .child(self.render_card(
                Page::Cleanup,
                IconName::Brush,
                Tone::Success,
                ui(lang, Key::NavCleanup),
                lang,
                cx,
            ));
        let cards_bottom = h_flex()
            .gap_4()
            .flex_wrap()
            .child(self.render_card(
                Page::Space,
                IconName::ChartPie,
                Tone::Info,
                ui(lang, Key::CardLargeFiles),
                lang,
                cx,
            ))
            .child(self.render_card(
                Page::Launch,
                IconName::Rocket,
                Tone::Danger,
                ui(lang, Key::NavLaunch),
                lang,
                cx,
            ));
        v_flex().size_full().child(header).child(
            div()
                .id("overview-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(
                    v_flex()
                        .px_6()
                        .pb_6()
                        .gap_4()
                        .child(self.render_disk(lang, cx))
                        .child(self.render_hero(lang, cx))
                        .child(
                            div()
                                .pt_2()
                                .text_sm()
                                .font_semibold()
                                .child(ui(lang, Key::OverviewCards)),
                        )
                        .child(cards_top)
                        .child(cards_bottom),
                ),
        )
    }
}
