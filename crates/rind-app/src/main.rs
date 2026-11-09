//! Rind 的桌面应用。`RIND_DEMO=1` 时用固定的示例数据，不碰真实系统。
//!
//! 外观优先级：`RIND_APPEARANCE=system|light|dark` > 上次在设置里选的那个（存在
//! `<data-dir>/appearance`）> 跟随系统。示例模式不读盘上的选择。

mod app;
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
use crate::strings::LangSetting;

actions!(rind, [Quit]);

fn main() {
    let demo = std::env::var_os("RIND_DEMO").is_some_and(|v| !v.is_empty() && v != "0");
    let services = Arc::new(Services::new(demo));

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(LangSetting::new(services.default_lang()));
        cx.set_global(CleanupPrefs::default());
        let appearance = std::env::var("RIND_APPEARANCE")
            .ok()
            .and_then(|value| Appearance::parse(value.trim()))
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
