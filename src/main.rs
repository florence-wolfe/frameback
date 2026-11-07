mod audio;
mod clipboard;
mod record;
mod update;
mod ui;
mod video;

use gpui::{App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

fn main() {
    Application::new().run(|cx: &mut App| {
        ui::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Frameback".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(320.), px(180.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ui::Frameback::new(window, cx)),
        )
        .expect("open window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
