//! Rind 的桌面应用。`RIND_DEMO=1` 时用固定的示例数据，不碰真实系统。
//!
//! 外观优先级：`RIND_APPEARANCE=system|light|dark` > 上次在设置里选的那个（存在
//! `<data-dir>/appearance`）> 跟随系统。示例模式不读盘上的选择。
//! 语言同理：`RIND_LANG=zh|en` > `<data-dir>/language` > 系统区域设置（`Lang::detect`）。
//! 两个环境变量都按「去掉空白后为空 = 没设」处理。

mod app;
mod app_assets;
mod apps_page;
mod cleanup_page;
mod demo;
mod history_page;
mod icons;
mod launch_page;
mod model;
mod orphans_page;
mod overview_page;
mod prefs;
mod services;
mod settings_page;
mod space_page;
mod store;
mod strings;
mod ui;

use std::sync::Arc;

use gpui_kit::*;

use crate::app::RindApp;
use crate::services::{CleanupPrefs, Services};
use crate::settings_page::Appearance;
use crate::strings::{Lang, LangSetting};

actions!(rind, [Quit]);

/// 启动时的界面语言：`RIND_LANG` >（真实模式）记住的选择 > 示例模式默认中文 / 系统区域。
fn resolve_lang(demo: bool) -> Lang {
    if let Some(value) = std::env::var("RIND_LANG")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        match Lang::from_code(&value) {
            Some(lang) => return lang,
            None => eprintln!("rind: 不认识 RIND_LANG={value:?}（想要 zh / en），忽略"),
        }
    }
    if demo {
        return Lang::ZhCn;
    }
    if let Some(lang) = prefs::load_lang(&rind_core::Env::detect().data_dir) {
        return lang;
    }
    Lang::detect()
}

fn main() {
    let demo = std::env::var_os("RIND_DEMO").is_some_and(|v| !v.is_empty() && v != "0");
    // 语言要先定下来：示例历史的清理批次标题按界面语言生成（和真实 GUI 写 ledger 的文案一致）。
    let lang = resolve_lang(demo);
    let services = Arc::new(Services::new(demo, lang));

    let app = gpui_kit::application().with_assets(app_assets::AppAssets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(CleanupPrefs::default());
        cx.set_global(LangSetting::new(lang));
        let appearance = std::env::var("RIND_APPEARANCE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| {
                let parsed = Appearance::parse(value.trim());
                if parsed.is_none() {
                    eprintln!(
                        "rind: 不认识 RIND_APPEARANCE={value:?}（想要 system / light / dark），忽略"
                    );
                }
                parsed
            })
            .or_else(|| {
                if services.is_demo() {
                    None
                } else {
                    prefs::load(&services.env().data_dir)
                }
            })
            .unwrap_or_default();
        cx.set_global(appearance);

        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1200.), px(780.)), cx)),
            window_min_size: Some(size(px(980.), px(640.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Rind".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("Rind");
            cx.new(|cx| RindApp::new(services.clone(), window, cx))
        })
        .expect("failed to open the Rind window");
        cx.activate(true);
    });
}
