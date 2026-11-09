//! 窗口里的根视图：左边原生风格的侧栏（总览 / 清理组 / 底部历史与设置），右边当前页。
//! 所有页面常驻，换页只换显示，不丢扫描结果。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::component_traits::Collapsible;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::apps_page::AppsPage;
use crate::cleanup_page::CleanupPage;
use crate::history_page::HistoryPage;
use crate::launch_page::LaunchPage;
use crate::model::{self, Page};
use crate::orphans_page::OrphansPage;
use crate::overview_page::{NavigateTo, OverviewPage};
use crate::services::Services;
use crate::settings_page::{Appearance, SettingsPage};
use crate::space_page::SpacePage;
use crate::store::Scans;
use crate::strings::{self, ui, Key, Lang, LangSetting};

/// 侧栏的一块：单独的菜单（总览），或带标题的一组（清理）。
#[derive(Clone)]
enum NavBlock {
    Menu(Box<SidebarMenu>),
    Group(SidebarGroup<SidebarMenu>),
}

impl Collapsible for NavBlock {
    fn collapsed(self, collapsed: bool) -> Self {
        match self {
            NavBlock::Menu(m) => NavBlock::Menu(Box::new((*m).collapsed(collapsed))),
            NavBlock::Group(g) => NavBlock::Group(g.collapsed(collapsed)),
        }
    }

    fn is_collapsed(&self) -> bool {
        match self {
            NavBlock::Menu(m) => m.is_collapsed(),
            NavBlock::Group(g) => g.is_collapsed(),
        }
    }
}

impl SidebarItem for NavBlock {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        match self {
            NavBlock::Menu(m) => (*m).render(id, window, cx).into_any_element(),
            NavBlock::Group(g) => g.render(id, window, cx).into_any_element(),
        }
    }
}

fn page_icon(page: Page) -> IconName {
    match page {
        Page::Overview => IconName::LayoutDashboard,
        Page::Apps => IconName::AppWindow,
        Page::Orphans => IconName::Ghost,
        Page::Cleanup => IconName::Brush,
        Page::Space => IconName::ChartPie,
        Page::Launch => IconName::Rocket,
        Page::History => IconName::ArchiveRestore,
        Page::Settings => IconName::Settings,
    }
}

fn page_label(lang: Lang, page: Page) -> &'static str {
    let key = match page {
        Page::Overview => Key::NavOverview,
        Page::Apps => Key::NavApps,
        Page::Orphans => Key::NavOrphans,
        Page::Cleanup => Key::NavCleanup,
        Page::Space => Key::NavSpace,
        Page::Launch => Key::NavLaunch,
        Page::History => Key::NavHistory,
        Page::Settings => Key::NavSettings,
    };
    ui(lang, key)
}

pub struct RindApp {
    page: Page,
    demo: bool,
    scans: Entity<Scans>,
    overview: Entity<OverviewPage>,
    apps: Entity<AppsPage>,
    orphans: Entity<OrphansPage>,
    cleanup: Entity<CleanupPage>,
    space: Entity<SpacePage>,
    launch: Entity<LaunchPage>,
    history: Entity<HistoryPage>,
    settings: Entity<SettingsPage>,
    _subscriptions: Vec<Subscription>,
}

impl RindApp {
    pub fn new(services: Arc<Services>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 外观：默认跟随系统，系统切换深浅色时跟着变。
        let appearance = cx.try_global::<Appearance>().copied().unwrap_or_default();
        appearance.apply(Some(window), cx);

        let scans = cx.new(|cx| Scans::new(services.clone(), cx));
        let overview = cx.new(|cx| OverviewPage::new(scans.clone(), cx));
        let apps = cx.new(|cx| AppsPage::new(services.clone(), scans.clone(), window, cx));
        let orphans = cx.new(|cx| OrphansPage::new(services.clone(), scans.clone(), cx));
        let cleanup = cx.new(|cx| CleanupPage::new(services.clone(), scans.clone(), cx));
        let space = cx.new(|cx| SpacePage::new(services.clone(), scans.clone(), cx));
        let launch = cx.new(|cx| LaunchPage::new(services.clone(), scans.clone(), cx));
        let history = cx.new(|cx| HistoryPage::new(services.clone(), cx));
        let settings = cx.new(|cx| SettingsPage::new(services.clone(), cx));
        let subscriptions = vec![
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
            cx.observe(&scans, |_, _, cx| cx.notify()),
            cx.subscribe(&overview, |this, _, event: &NavigateTo, cx| {
                this.show(event.0, cx)
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if cx.try_global::<Appearance>().copied().unwrap_or_default() == Appearance::System
                {
                    Appearance::System.apply(Some(window), cx);
                }
            }),
        ];
        // 开发辅助：`RIND_AUTOSCAN=1` 启动即跑智能扫描和家目录的空间分析，`RIND_PAGE=space` 等指定首页。
        if std::env::var_os("RIND_AUTOSCAN").is_some_and(|v| v == "1") {
            let prefs = crate::store::cleanup_prefs(cx);
            let home = services.env().home.clone();
            scans.update(cx, |s, cx| {
                s.smart_scan(prefs, cx);
                s.analyze_space(home, cx);
            });
        }
        let mut app = Self {
            page: Page::Overview,
            demo: services.is_demo(),
            scans,
            overview,
            apps,
            orphans,
            cleanup,
            space,
            launch,
            history,
            settings,
            _subscriptions: subscriptions,
        };
        if let Some(page) = std::env::var("RIND_PAGE")
            .ok()
            .and_then(|name| model::page_from_name(&name))
        {
            app.show(page, cx);
        }
        app
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        match page {
            Page::History => self.history.update(cx, |h, cx| h.refresh(cx)),
            Page::Launch => self.launch.update(cx, |l, cx| l.ensure_loaded(cx)),
            _ => {}
        }
        cx.notify();
    }

    /// 侧栏条目右边的小数字：有结果才显示。
    fn badge(&self, page: Page, lang: Lang, cx: &App) -> Option<String> {
        let scans = self.scans.read(cx);
        match page {
            Page::Apps => scans.apps.data.as_ref().map(|a| a.len().to_string()),
            Page::Orphans => scans
                .orphans
                .data
                .as_ref()
                .filter(|r| !r.groups.is_empty())
                .map(|r| r.groups.len().to_string()),
            Page::Cleanup => scans
                .cleanup
                .data
                .as_ref()
                .map(|r| model::cleanup_suggested_bytes(r))
                .filter(|n| *n > 0)
                .map(|n| crate::ui::bytes(lang, n)),
            Page::Launch => scans
                .launch
                .data
                .as_ref()
                .map(|items| items.iter().filter(|i| i.orphan).count())
                .filter(|n| *n > 0)
                .map(|n| n.to_string()),
            _ => None,
        }
    }

    fn menu_item(&self, page: Page, lang: Lang, cx: &mut Context<Self>) -> SidebarMenuItem {
        let badge = self.badge(page, lang, cx);
        SidebarMenuItem::new(page_label(lang, page))
            .icon(Icon::new(page_icon(page)))
            .active(self.page == page)
            .when_some(badge, |item, badge| {
                item.suffix(move |_, cx| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(badge.clone())
                })
            })
            .on_click(cx.listener(move |this, _, _, cx| this.show(page, cx)))
    }

    fn render_brand(&self, lang: Lang, cx: &App) -> impl IntoElement {
        let primary = cx.theme().primary;
        h_flex()
            .w_full()
            .gap_2p5()
            .pb_1()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .size(px(30.))
                    .justify_center()
                    .rounded(cx.theme().radius_lg)
                    .bg(primary)
                    .text_color(cx.theme().primary_foreground)
                    .child(Icon::new(IconName::Citrus).small()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_semibold().child(ui(lang, Key::AppTitle)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
                            .truncate()
                            .child(ui(lang, Key::AppTagline)),
                    ),
            )
            .when(self.demo, |this| {
                this.child(Tag::warning().xsmall().child(ui(lang, Key::DemoBadge)))
            })
    }
}

impl Render for RindApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let content: AnyElement = match self.page {
            Page::Overview => self.overview.clone().into_any_element(),
            Page::Apps => self.apps.clone().into_any_element(),
            Page::Orphans => self.orphans.clone().into_any_element(),
            Page::Cleanup => self.cleanup.clone().into_any_element(),
            Page::Space => self.space.clone().into_any_element(),
            Page::Launch => self.launch.clone().into_any_element(),
            Page::History => self.history.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        };
        let overview = NavBlock::Menu(Box::new(SidebarMenu::new().child(self.menu_item(
            Page::Overview,
            lang,
            cx,
        ))));
        let clean_items: Vec<SidebarMenuItem> = Page::CLEAN
            .iter()
            .map(|&page| self.menu_item(page, lang, cx))
            .collect();
        let clean = NavBlock::Group(
            SidebarGroup::new(ui(lang, Key::NavGroupClean))
                .child(SidebarMenu::new().children(clean_items)),
        );
        let footer_items: Vec<SidebarMenuItem> = Page::FOOTER
            .iter()
            .map(|&page| self.menu_item(page, lang, cx))
            .collect();
        let footer = SidebarMenu::new()
            .w_full()
            .children(footer_items)
            .render("footer-nav", window, cx)
            .into_any_element();

        h_flex()
            .items_stretch()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                Sidebar::new("navigation")
                    .w(px(228.))
                    .collapsible(false)
                    .header(self.render_brand(lang, cx))
                    .child(overview)
                    .child(clean)
                    .footer(footer),
            )
            .child(div().flex_1().min_w_0().h_full().child(content))
    }
}
