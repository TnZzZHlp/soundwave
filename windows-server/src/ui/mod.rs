mod calibration_window;
mod controls;
mod dashboard;
mod model;
mod name_input;
mod native;
mod qr;
mod tray;

use std::{cell::RefCell, rc::Rc, sync::mpsc};

use gpui::{prelude::*, *};

pub(crate) use native::{show_capture_complete, show_startup_error};

pub(crate) fn run(model: crate::application::state::ServerState) -> anyhow::Result<()> {
    let shutdown = model.shutdown.clone();
    let volume = model.volume.clone();
    let (sender, receiver) = mpsc::sync_channel(16);
    let tray = tray::Tray::new(sender, model.transmission.clone())?;
    let failure = Rc::new(RefCell::new(None));
    let startup_failure = failure.clone();
    Application::new().run(move |cx| {
        let bounds = Bounds::centered(None, size(px(1040.), px(820.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(920.), px(620.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Soundwave 服务端".into()),
                    ..Default::default()
                }),
                app_id: Some("Soundwave.Server".into()),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| dashboard::Dashboard::new(model, tray, receiver, window, cx))
            },
        );
        match result {
            Ok(handle) => {
                let _ = handle.update(cx, |_, window, _| native::show(window));
            }
            Err(error) => {
                *startup_failure.borrow_mut() = Some(error);
                cx.quit();
            }
        }
    });
    shutdown.cancel();
    let volume_result = volume.finish();
    let error = failure.borrow_mut().take();
    error.map_or(Ok(()), Err).and(volume_result)
}
