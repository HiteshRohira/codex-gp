use codex_gpui_desktop::ui::{RootView, bind_keys};
use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.activate(true);
        bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(RootView::new),
        );

        if let Err(error) = window {
            eprintln!("failed to open Codex GPUI Desktop: {error}");
            cx.quit();
        }
    });
}
