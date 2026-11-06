//! Rind 的桌面应用。`RIND_DEMO=1` 时用固定的示例数据，不碰真实系统。

mod app;
mod apps_page;
mod demo;
mod history_page;
mod services;
mod settings_page;
mod strings;

use std::sync::Arc;

use gpui_kit::*;

use crate::app::RindApp;
use crate::services::Services;
use crate::strings::LangSetting;

actions!(rind, [Quit]);

fn main() {
    let demo = std::env::var_os("RIND_DEMO").is_some_and(|v| !v.is_empty() && v != "0");
    let services = Arc::new(Services::new(demo));

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(LangSetting::new(services.default_lang()));

        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1120.), px(720.)), cx)),
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
