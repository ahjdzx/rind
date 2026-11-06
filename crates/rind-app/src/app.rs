//! 窗口里的根视图：左边导航，右边当前页。

use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::sidebar::{Sidebar, SidebarHeader, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, ActiveTheme as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::apps_page::AppsPage;
use crate::history_page::HistoryPage;
use crate::services::Services;
use crate::settings_page::SettingsPage;
use crate::strings::{self, ui, Key, LangSetting};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Apps,
    History,
    Settings,
}

pub struct RindApp {
    page: Page,
    demo: bool,
    apps: Entity<AppsPage>,
    history: Entity<HistoryPage>,
    settings: Entity<SettingsPage>,
    _subscriptions: Vec<Subscription>,
}

impl RindApp {
    pub fn new(services: Arc<Services>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let apps = cx.new(|cx| AppsPage::new(services.clone(), window, cx));
        let history = cx.new(|cx| HistoryPage::new(services.clone(), cx));
        let settings = cx.new(|cx| SettingsPage::new(services.clone(), cx));
        let subscriptions = vec![cx.observe_global::<LangSetting>(|_, cx| cx.notify())];
        Self {
            page: Page::Apps,
            demo: services.is_demo(),
            apps,
            history,
            settings,
            _subscriptions: subscriptions,
        }
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        if page == Page::History {
            self.history.update(cx, |history, cx| history.refresh(cx));
        }
        cx.notify();
    }

    fn menu_item(
        &self,
        page: Page,
        label: &'static str,
        icon: Icon,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        SidebarMenuItem::new(label)
            .icon(icon)
            .active(self.page == page)
            .on_click(cx.listener(move |this, _, _, cx| this.show(page, cx)))
    }
}

impl Render for RindApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let content: AnyElement = match self.page {
            Page::Apps => self.apps.clone().into_any_element(),
            Page::History => self.history.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        };
        h_flex()
            .items_stretch()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                Sidebar::new("navigation")
                    .w_48()
                    .header(
                        SidebarHeader::new().child(
                            h_flex()
                                .gap_2()
                                .child(Icon::new(LucideIcon::Eraser))
                                .child(div().font_bold().child(ui(lang, Key::AppTitle)))
                                .when(self.demo, |this| {
                                    this.child(
                                        Tag::warning().xsmall().child(ui(lang, Key::DemoBadge)),
                                    )
                                }),
                        ),
                    )
                    .child(SidebarMenu::new().children([
                        self.menu_item(
                            Page::Apps,
                            ui(lang, Key::NavApps),
                            Icon::new(LucideIcon::AppWindow),
                            cx,
                        ),
                        self.menu_item(
                            Page::History,
                            ui(lang, Key::NavHistory),
                            Icon::new(IconName::Undo2),
                            cx,
                        ),
                        self.menu_item(
                            Page::Settings,
                            ui(lang, Key::NavSettings),
                            Icon::new(IconName::Settings),
                            cx,
                        ),
                    ])),
            )
            .child(div().flex_1().min_w_0().h_full().child(content))
    }
}
