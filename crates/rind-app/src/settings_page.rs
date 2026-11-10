//! 设置页：外观与语言（都会记住）、缓存清理的偏好，以及 Rind 永远不动的地方。

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _, Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::services::{CleanupPrefs, ProtectedRow, Services};
use crate::store;
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui;

/// 外观：跟随系统、浅色、深色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Global for Appearance {}

impl Appearance {
    const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    /// 落盘用的名字。改了要当作兼容性变更（旧文件会认不出来，落回跟随系统）。
    pub fn as_str(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }

    /// 从落盘的名字解析；认不出来返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "system" => Some(Appearance::System),
            "light" => Some(Appearance::Light),
            "dark" => Some(Appearance::Dark),
            _ => None,
        }
    }

    /// 套用到主题上。
    pub fn apply(self, window: Option<&mut Window>, cx: &mut App) {
        match self {
            Appearance::System => {
                // 清掉覆盖，窗口（包括原生标题栏、边框）重新跟系统走。
                cx.set_window_appearance(None);
                Theme::sync_system_appearance(window, cx);
            }
            Appearance::Light => {
                cx.set_window_appearance(Some(WindowAppearance::Light));
                Theme::change(ThemeMode::Light, window, cx);
            }
            Appearance::Dark => {
                cx.set_window_appearance(Some(WindowAppearance::Dark));
                Theme::change(ThemeMode::Dark, window, cx);
            }
        }
        cx.refresh_windows();
    }
}

/// 可选的"多少天没动过"。
const AGE_CHOICES: [u32; 5] = [7, 14, 30, 60, 90];

pub struct SettingsPage {
    services: Arc<Services>,
    protected: Vec<ProtectedRow>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(services: Arc<Services>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_global::<LangSetting>(|this, cx| this.load_protected(cx)),
            cx.observe_global::<CleanupPrefs>(|_, cx| cx.notify()),
        ];
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
        if !self.services.is_demo() {
            if let Err(err) = crate::prefs::save_lang(&self.services.env().data_dir, lang) {
                eprintln!("rind: 保存语言偏好失败：{err}");
            }
        }
        cx.notify();
    }

    fn set_prefs(&mut self, prefs: CleanupPrefs, cx: &mut Context<Self>) {
        cx.set_global(prefs);
        cx.notify();
    }

    /// 自定义目录列表：还没改过时以默认目录为起点。
    fn editable_roots(&self, prefs: &CleanupPrefs) -> Vec<PathBuf> {
        if prefs.roots.is_empty() {
            self.services.default_project_roots()
        } else {
            prefs.roots.clone()
        }
    }

    fn add_root(&mut self, cx: &mut Context<Self>) {
        let lang = strings::lang(cx);
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(ui(lang, Key::SettingsAddRoot).into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |this, cx| {
                    let mut prefs = store::cleanup_prefs(cx);
                    let mut roots = this.editable_roots(&prefs);
                    for path in paths {
                        if !roots.contains(&path) {
                            roots.push(path);
                        }
                    }
                    prefs.roots = roots;
                    this.set_prefs(prefs, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn remove_root(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let mut prefs = store::cleanup_prefs(cx);
        let mut roots = self.editable_roots(&prefs);
        roots.retain(|r| r != &path);
        prefs.roots = roots;
        self.set_prefs(prefs, cx);
    }

    fn section(title: &'static str, icon: IconName, cx: &App) -> Div {
        ui::card(cx).p_5().gap_3().child(
            h_flex()
                .gap_2()
                .child(Icon::new(icon).small().text_color(ui::muted(cx)))
                .child(div().text_sm().font_semibold().child(title)),
        )
    }

    fn render_general(&self, lang: Lang, cx: &mut Context<Self>) -> Div {
        let appearance = cx.try_global::<Appearance>().copied().unwrap_or_default();
        let lang_ix = match lang {
            Lang::ZhCn => 0,
            Lang::En => 1,
        };
        let appearance_ix = Appearance::ALL.iter().position(|a| *a == appearance);
        Self::section(ui(lang, Key::SettingsGeneral), IconName::Settings, cx)
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        div()
                            .w(px(120.))
                            .text_sm()
                            .child(ui(lang, Key::SettingsAppearance)),
                    )
                    .child(
                        RadioGroup::horizontal("appearance")
                            .children([
                                ui(lang, Key::AppearanceSystem),
                                ui(lang, Key::AppearanceLight),
                                ui(lang, Key::AppearanceDark),
                            ])
                            .selected_index(appearance_ix)
                            .on_change(cx.listener(|this, ix: &usize, window, cx| {
                                let appearance =
                                    Appearance::ALL.get(*ix).copied().unwrap_or_default();
                                // 点已选中的项、或组件同步状态时的重复事件，都不做事。
                                if cx.try_global::<Appearance>().copied().unwrap_or_default()
                                    == appearance
                                {
                                    return;
                                }
                                cx.set_global(appearance);
                                appearance.apply(Some(window), cx);
                                if !this.services.is_demo() {
                                    if let Err(err) = crate::prefs::save(
                                        &this.services.env().data_dir,
                                        appearance,
                                    ) {
                                        eprintln!("rind: 保存外观偏好失败：{err}");
                                    }
                                }
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        div()
                            .w(px(120.))
                            .text_sm()
                            .child(ui(lang, Key::SettingsLanguage)),
                    )
                    .child(
                        RadioGroup::horizontal("language")
                            .children([ui(lang, Key::LangZh), ui(lang, Key::LangEn)])
                            .selected_index(Some(lang_ix))
                            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                let lang = if *ix == 1 { Lang::En } else { Lang::ZhCn };
                                this.set_lang(lang, cx);
                            })),
                    ),
            )
    }

    fn render_cleanup(&self, lang: Lang, cx: &mut Context<Self>) -> Div {
        let muted = ui::muted(cx);
        let prefs = store::cleanup_prefs(cx);
        let custom = !prefs.roots.is_empty();
        let roots = self.editable_roots(&prefs);
        let env = self.services.env().clone();
        let age_ix = AGE_CHOICES.iter().position(|d| *d == prefs.min_age_days);
        let age_labels: Vec<SharedString> = AGE_CHOICES
            .iter()
            .map(|d| strings::days(lang, *d).into())
            .collect();
        Self::section(ui(lang, Key::SettingsCleanup), IconName::Brush, cx)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(ui(lang, Key::SettingsSessionOnly)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(ui(lang, Key::SettingsRoots)))
                            .when(custom, |this| {
                                this.child(
                                    Button::new("roots-reset")
                                        .xsmall()
                                        .ghost()
                                        .label(ui(lang, Key::SettingsResetRoots))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            let mut prefs = store::cleanup_prefs(cx);
                                            prefs.roots.clear();
                                            this.set_prefs(prefs, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("roots-add")
                                    .xsmall()
                                    .outline()
                                    .icon(Icon::new(IconName::FolderOpen))
                                    .label(ui(lang, Key::SettingsAddRoot))
                                    .on_click(cx.listener(|this, _, _, cx| this.add_root(cx))),
                            ),
                    )
                    .child(
                        v_flex()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().border)
                            .when(roots.is_empty(), |this| {
                                this.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(ui(lang, Key::SettingsNoRoots)),
                                )
                            })
                            .children(roots.into_iter().enumerate().map(|(ix, root)| {
                                let remove = root.clone();
                                h_flex()
                                    .gap_2()
                                    .px_3()
                                    .py_1p5()
                                    .text_sm()
                                    .when(ix > 0, |this| {
                                        this.border_t_1().border_color(cx.theme().border)
                                    })
                                    .child(Icon::new(IconName::Folder).small().text_color(muted))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(env.display(&root)),
                                    )
                                    .when(!custom, |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(ui(lang, Key::SettingsDefault)),
                                        )
                                    })
                                    .child(
                                        Button::new(("root-remove", ix))
                                            .xsmall()
                                            .ghost()
                                            .icon(Icon::new(IconName::X))
                                            .tooltip(ui(lang, Key::SettingsRemoveRoot))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.remove_root(remove.clone(), cx)
                                            })),
                                    )
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        div()
                            .w(px(120.))
                            .text_sm()
                            .child(ui(lang, Key::SettingsAge)),
                    )
                    .child(
                        RadioGroup::horizontal("min-age")
                            .children(age_labels)
                            .selected_index(age_ix)
                            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                let mut prefs = store::cleanup_prefs(cx);
                                prefs.min_age_days = AGE_CHOICES.get(*ix).copied().unwrap_or(30);
                                this.set_prefs(prefs, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(ui(lang, Key::SettingsAgeHint)),
            )
    }

    fn render_protected(&self, lang: Lang, cx: &App) -> Div {
        let muted = ui::muted(cx);
        Self::section(ui(lang, Key::ProtectedTitle), IconName::ShieldCheck, cx)
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(ui(lang, Key::ProtectedDescription)),
            )
            .child(
                v_flex().children(self.protected.iter().enumerate().map(|(ix, row)| {
                    h_flex()
                        .gap_3()
                        .py_1p5()
                        .text_sm()
                        .when(ix > 0, |this| {
                            this.border_t_1().border_color(cx.theme().border)
                        })
                        .child(div().w(px(320.)).truncate().child(row.path.clone()))
                        .child(
                            div()
                                .flex_1()
                                .text_color(muted)
                                .truncate()
                                .child(row.reason.clone()),
                        )
                })),
            )
    }
}

impl SettingsPage {
    fn render_about(&self, lang: Lang, cx: &App) -> Div {
        let muted = ui::muted(cx);
        Self::section(ui(lang, Key::AboutTitle), IconName::Info, cx).child(
            h_flex()
                .gap_4()
                .items_center()
                .child(ui::brand_logo(px(56.)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_baseline()
                                .child(
                                    div()
                                        .text_base()
                                        .font_semibold()
                                        .child(ui(lang, Key::AppTitle)),
                                )
                                .child(div().text_xs().text_color(muted).child(format!(
                                    "{} {}",
                                    ui(lang, Key::AboutVersion),
                                    env!("CARGO_PKG_VERSION")
                                ))),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(ui(lang, Key::AboutDescription)),
                        ),
                ),
        )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let header = ui::page_header(
            IconName::Settings,
            ui(lang, Key::NavSettings),
            ui(lang, Key::SettingsSubtitle),
            div(),
            cx,
        );
        v_flex().size_full().child(header).child(
            div()
                .id("settings-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(
                    v_flex()
                        .px_6()
                        .pb_6()
                        .gap_4()
                        .child(self.render_general(lang, cx))
                        .child(self.render_cleanup(lang, cx))
                        .child(self.render_protected(lang, cx))
                        .child(self.render_about(lang, cx)),
                ),
        )
    }
}
