use std::time::Duration;

use gpui::{prelude::*, *};

use super::{
    controls::SliderKind, dashboard::Dashboard, model::Command, name_input::NameInput, native,
};

pub(super) struct CalibrationWindow {
    owner: Entity<Dashboard>,
    name: Entity<NameInput>,
    loaded: Option<(u64, String, String)>,
    _poll: Task<()>,
}

impl CalibrationWindow {
    fn new(owner: Entity<Dashboard>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let parent = owner.clone();
        window.on_window_should_close(cx, move |_, cx| {
            parent.update(cx, |parent, _| {
                if parent.controller.server.calibration.status().0 {
                    parent.controller.server.calibration.cancel();
                    parent.controller.notice = "正在取消测量并恢复音量，请稍后再次关闭窗口".into();
                    return false;
                }
                parent.controller.command(Command::CloseCalibration);
                if parent.controller.view().editing {
                    false
                } else {
                    parent.calibration_window = None;
                    true
                }
            })
        });
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if this.update_in(cx, |_, _, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            owner,
            name: cx.new(NameInput::new),
            loaded: None,
            _poll: poll,
        }
    }
}

impl Render for CalibrationWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draft = self.owner.read(cx).controller.view().draft;
        let key = draft
            .as_ref()
            .map(|p| (p.id, p.master.id.clone(), p.name.clone()));
        if key != self.loaded {
            self.name.update(cx, |input, cx| {
                input.set(
                    draft.as_ref().map_or_else(String::new, |p| p.name.clone()),
                    cx,
                )
            });
            self.loaded = key;
        }
        let name = self.name.clone();
        self.owner
            .update(cx, |parent, cx| parent.calibration_content(name, cx))
    }
}

impl Dashboard {
    pub(super) fn open_calibration(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.calibration_window
            && handle
                .update(cx, |_, window, _| native::show(window))
                .is_ok()
        {
            return;
        }
        self.controller.command(Command::OpenCalibration);
        if !self.controller.view().editing {
            cx.notify();
            return;
        }
        let owner = cx.entity();
        let bounds = Bounds::centered(None, size(px(1000.), px(800.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(780.), px(560.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("主从音量校准".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| CalibrationWindow::new(owner, window, cx)),
        );
        match result {
            Ok(handle) => self.calibration_window = Some(handle),
            Err(error) => {
                self.controller.command(Command::CloseCalibration);
                self.controller.notice = format!("无法打开校准窗口：{error}");
            }
        }
        cx.notify();
    }

    fn calibration_content(&self, name: Entity<NameInput>, cx: &Context<Self>) -> Stateful<Div> {
        let c = &self.controller;
        let view = c.view();
        let (busy, progress) = c.server.calibration.status();
        let ready = !busy && c.settled();
        let draft = view.draft.as_ref();
        let save_name = name.clone();
        let copy_name = name.clone();
        let mut profiles = div().flex().flex_wrap().gap_2();
        for profile in &view.library.profiles {
            profiles = profiles.child(
                self.button(
                    ("edit-profile", profile.id),
                    profile.name.clone(),
                    ready,
                    Command::Profile(profile.id),
                    cx,
                )
                .when(draft.is_some_and(|p| p.id == profile.id), |el| {
                    el.border_color(self.colors.accent)
                }),
            );
        }
        let mut devices = div().flex().flex_col().gap_3();
        for receiver in &c.receivers {
            let Some(device) = &receiver.device else {
                continue;
            };
            let master = draft.is_some_and(|p| p.master.id == device.device_id);
            let follower =
                draft.and_then(|p| p.followers.iter().find(|f| f.device.id == device.device_id));
            let volume = c.volume(receiver);
            let mut row = self
                .panel()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(format!(
                            "{} · {} · {}/{}",
                            device.name,
                            if master { "主设备" } else { "从设备" },
                            volume,
                            device.maximum
                        ))
                        .child(self.button(
                            ("set-master", receiver.id),
                            if master {
                                "刷新从设备"
                            } else {
                                "设为主设备"
                            },
                            ready && device.speaker && !device.fixed,
                            Command::SetMaster(receiver.id),
                            cx,
                        )),
                )
                .child(self.slider(
                    ("edit-volume", receiver.id),
                    SliderKind::Device(receiver.id),
                    volume,
                    (device.minimum, device.maximum),
                    !busy && device.speaker && !device.fixed && !device.failed,
                    cx,
                ))
                .child(self.button(
                    ("preview-device", receiver.id),
                    "试听 5 秒",
                    ready && device.speaker && !device.fixed,
                    Command::PreviewDevice(receiver.id),
                    cx,
                ));
            if let Some(follower) = follower {
                let point = follower
                    .mapping
                    .get(&c.point)
                    .map_or_else(|| "未校准".into(), |index| index.to_string());
                row = row.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(format!("主设备 {} → 从设备 {}", c.point, point))
                        .child(self.button(
                            ("record-point", receiver.id),
                            "记录当前对应",
                            ready && follower.device.matches(device),
                            Command::Record(receiver.id),
                            cx,
                        ))
                        .child(self.button(
                            ("remove-point", receiver.id),
                            "删除对应",
                            ready && follower.mapping.contains_key(&c.point),
                            Command::RemovePoint(receiver.id),
                            cx,
                        )),
                );
                let points = follower
                    .mapping
                    .iter()
                    .map(|(master, slave)| format!("{master}→{slave}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                row = row.child(div().text_xs().text_color(self.colors.muted).child(points));
            }
            devices = devices.child(row);
        }
        let mut content =
            div()
                .id("calibration-editor")
                .size_full()
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .bg(self.colors.background)
                .text_color(self.colors.text)
                .font_family("Microsoft YaHei UI")
                .text_sm()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.dragging = None),
                )
                .on_key_down(|event, window, cx| {
                    if event.keystroke.key == "tab" {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        cx.stop_propagation();
                    }
                })
                .child(div().text_xl().child("主从音量校准"))
                .child(profiles)
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(self.button("new-profile", "新配置", ready, Command::NewProfile, cx))
                        .child(self.button("stop-tone", "停止试听", !busy, Command::StopTone, cx))
                        .child(self.button("cancel-auto", "取消测量", busy, Command::Cancel, cx)),
                )
                .child(div().text_color(self.colors.muted).child(
                    "校准窗口内可自由调节。正式播放只使用保存的对应表；未校准设备不参与播放。",
                ));
        if let Some(draft) = draft {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(format!(
                        "主设备：{} · 当前校准档位 {}",
                        draft.master.name, c.point
                    ))
                    .child(self.slider(
                        "master-point",
                        SliderKind::MasterPoint,
                        c.point,
                        (draft.master.minimum, draft.master.maximum),
                        !busy,
                        cx,
                    )),
            );
        } else {
            content = content.child("请从下方选择一台主设备，其他已连接设备将加入从设备列表。");
        }
        content
            .child(
                div()
                    .id("calibration-devices")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(devices),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child("配置名称")
                    .child(div().flex_1().child(name)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.button(
                        "auto-calibrate",
                        "自动生成对应表",
                        ready && draft.is_some(),
                        Command::Calibrate,
                        cx,
                    ))
                    .child(self.action_button(
                        "save-profile",
                        "保存并应用",
                        ready && draft.is_some(),
                        move |this, window, cx| {
                            let text = save_name.read(cx).text.clone();
                            this.dispatch(Command::SaveProfile(text, false), window, cx);
                        },
                        cx,
                    ))
                    .child(self.action_button(
                        "copy-profile",
                        "另存为新配置",
                        ready && draft.is_some(),
                        move |this, window, cx| {
                            let text = copy_name.read(cx).text.clone();
                            this.dispatch(Command::SaveProfile(text, true), window, cx);
                        },
                        cx,
                    ))
                    .child(self.button(
                        "delete-profile",
                        "删除配置",
                        ready && draft.is_some_and(|p| p.id != 0),
                        Command::DeleteProfile,
                        cx,
                    ))
                    .child(self.button(
                        "close-calibration",
                        "关闭",
                        ready,
                        Command::CloseCalibration,
                        cx,
                    )),
            )
            .child(div().text_color(self.colors.muted).child(if busy {
                progress
            } else if !c.notice.is_empty() {
                c.notice.clone()
            } else if !progress.is_empty() {
                progress
            } else {
                view.message
            }))
    }
}
