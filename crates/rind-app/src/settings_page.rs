//! 设置页：语言，以及 Rind 永远不动的地方。

use std::sync::Arc;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::services::{ProtectedRow, Services};
use crate::strings::{self, ui, Key, Lang, LangSetting};

pub struct SettingsPage {
    services: Arc<Services>,
    protected: Vec<ProtectedRow>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(services: Arc<Services>, cx: &mut Context<Self>) -> Self {
        let subscriptions =
            vec![cx.observe_global::<LangSetting>(|this, cx| this.load_protected(cx))];
        let mut page = Self {
            services,
            protected: Vec::new(),
            _subscriptions: subscriptions,
        };
        page.load_protected(cx);
        page
    }

    fn load_protected(&mut self, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let lang = strings::lang(cx);
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_spawn(async move { services.protected(lang) })
                .await;
            this.update(cx, |this, cx| {
                this.protected = rows;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_lang(&mut self, lang: Lang, cx: &mut Context<Self>) {
        if strings::lang(cx) == lang {
            return;
        }
        cx.set_global(LangSetting::new(lang));
        cx.notify();
    }

    fn render_language(&self, lang: Lang, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match lang {
            Lang::ZhCn => 0,
            Lang::En => 1,
        };
        v_flex()
            .gap_2()
            .child(div().font_semibold().child(ui(lang, Key::SettingsLanguage)))
            .child(
                RadioGroup::horizontal("language")
                    .children([ui(lang, Key::LangZh), ui(lang, Key::LangEn)])
                    .selected_index(Some(selected))
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        let lang = if *ix == 1 { Lang::En } else { Lang::ZhCn };
                        this.set_lang(lang, cx);
                    })),
            )
    }

    fn render_protected(&self, lang: Lang, cx: &Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(gpui_kit::assets::IconName::ShieldCheck).small())
                    .child(div().font_semibold().child(ui(lang, Key::ProtectedTitle))),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(ui(lang, Key::ProtectedDescription)),
            )
            .child(
                div()
                    .id("protected-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .pr_2()
                            .children(self.protected.iter().enumerate().map(|(ix, row)| {
                                h_flex()
                                    .gap_3()
                                    .py_1()
                                    .text_sm()
                                    .when(ix > 0, |this| {
                                        this.border_t_1().border_color(cx.theme().border)
                                    })
                                    .child(div().w_80().truncate().child(row.path.clone()))
                                    .child(div().text_color(muted).child(row.reason.clone()))
                            })),
                    ),
            )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        v_flex()
            .size_full()
            .p_4()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_bold()
                    .child(ui(lang, Key::SettingsTitle)),
            )
            .child(self.render_language(lang, cx))
            .child(self.render_protected(lang, cx))
    }
}
