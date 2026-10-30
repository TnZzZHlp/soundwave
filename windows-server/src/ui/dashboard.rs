use std::{sync::mpsc::Receiver, time::Duration};

use gpui::{prelude::*, *};
use qrcodegen::{QrCode, QrCodeEcc};

use super::{
    controls::SliderKind,
    model::{Command, Controller, Page},
    native, qr,
    tray::Tray,
};

#[derive(Clone, Copy)]
pub(super) struct Colors {
    pub background: Hsla,
    pub panel: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub hover: Hsla,
}

impl Colors {
    fn for_window(window: &Window) -> Self {
        let dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        if dark {
            Self {
                background: rgb(0x17191d).into(),
                panel: rgb(0x22252b).into(),
                text: rgb(0xf1f3f5).into(),
                muted: rgb(0xaeb5c0).into(),
                border: rgb(0x3d424b).into(),
                accent: rgb(0x73b5ff).into(),
                hover: rgb(0x303640).into(),
            }
        } else {
            Self {
                background: rgb(0xf4f6f8).into(),
                panel: rgb(0xffffff).into(),
                text: rgb(0x1b2430).into(),
                muted: rgb(0x576373).into(),
                border: rgb(0xd7dde5).into(),
                accent: rgb(0x1769c2).into(),
                hover: rgb(0xe8f1fc).into(),
            }
        }
    }
}

pub(super) struct Dashboard {
    controller: Controller,
    matrix: Result<QrCode, String>,
    pub colors: Colors,
    pub dragging: Option<SliderKind>,
    _tray: Tray,
    _poll: Task<()>,
}

impl Dashboard {
    pub fn new(
        model: crate::application::state::ServerState,
        tray: Tray,
        receiver: Receiver<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let matrix = model
            .info
            .pairing
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|p| {
                QrCode::encode_text(&p.uri, QrCodeEcc::Medium)
                    .map_err(|error| format!("二维码生成失败：{error:?}"))
            });
        window.on_window_should_close(cx, |window, _| {
            native::hide(window);
            false
        });
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let result = this.update_in(cx, |this, window, cx| {
                    for command in receiver.try_iter().take(32) {
                        if matches!(command, Command::Show(_)) {
                            native::show(window);
                        }
                        this.dispatch(command, window, cx);
                    }
                    this.controller.refresh();
                    if this.controller.server.shutdown.is_cancelled() {
                        this.controller.server.calibration.cancel();
                        cx.quit();
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(result, Ok(true)) {
                    break;
                }
            }
        });
        Self {
            controller: Controller::new(model),
            matrix,
            colors: Colors::for_window(window),
            dragging: None,
            _tray: tray,
            _poll: poll,
        }
    }

    pub(super) fn dispatch(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &command {
            Command::CopyFingerprint => cx.write_to_clipboard(ClipboardItem::new_string(
                self.controller.server.info.fingerprint.clone(),
            )),
            Command::CopyPairing => {
                if let Ok(pairing) = &self.controller.server.info.pairing {
                    cx.write_to_clipboard(ClipboardItem::new_string(pairing.uri.clone()));
                }
            }
            Command::Calibrate if !native::confirm_calibration(window) => return,
            Command::Show(_) => self.dragging = None,
            _ => {}
        }
        self.controller.command(command);
        cx.notify();
    }

    fn panel(&self) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .bg(self.colors.panel)
            .border_1()
            .border_color(self.colors.border)
            .rounded_lg()
    }

    fn pairing(&self, cx: &Context<Self>) -> Div {
        let info = &self.controller.server.info;
        let mut panel = self
            .panel()
            .w(px(342.))
            .flex_shrink_0()
            .child(div().text_lg().child("配对手机"));
        panel = match &self.matrix {
            Ok(matrix) => panel.child(qr::render(matrix.clone())),
            Err(error) => panel.child(div().text_color(self.colors.muted).child(error.clone())),
        };
        if let Ok(pairing) = &info.pairing {
            panel = panel.child(pairing.endpoint.to_string()).child(self.button(
                "copy-pairing",
                "复制配对链接",
                true,
                Command::CopyPairing,
                cx,
            ));
        }
        panel.child(
            div()
                .text_sm()
                .text_color(self.colors.muted)
                .child("在 Audio Stream 中扫码，然后核对连接信息。"),
        )
    }

    fn receivers(&self, cx: &Context<Self>) -> Div {
        let c = &self.controller;
        let (busy, calibration_status) = c.server.calibration.status();
        let can_adjust = c.server.calibration.can_adjust();
        let mut list = div()
            .id("receiver-list")
            .max_h(px(180.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        for receiver in &c.receivers {
            let label = match &receiver.device {
                Some(device) => format!(
                    "{}  |  {}  |  {}/{}{}{}",
                    device.name,
                    receiver.peer,
                    device.volume,
                    device.maximum,
                    if device.fixed {
                        "（固定音量）"
                    } else {
                        ""
                    },
                    if device.failed {
                        "（请求被拒绝）"
                    } else {
                        ""
                    }
                ),
                None => format!("{}  |  等待设备状态", receiver.peer),
            };
            list = list.child(
                self.button(
                    ("receiver", receiver.id),
                    label,
                    !busy,
                    Command::Select(receiver.id),
                    cx,
                )
                .when(c.selected == Some(receiver.id), |el| {
                    el.border_color(self.colors.accent)
                }),
            );
        }
        if c.receivers.is_empty() {
            list = list.child(
                div()
                    .py_3()
                    .text_color(self.colors.muted)
                    .child("等待 Android 设备连接"),
            );
        }
        let device = c.selected_device();
        let volume = c.volume().unwrap_or(0);
        let (minimum, maximum) = device.map_or((0, 1), |d| (d.minimum, d.maximum));
        let label = device.map_or_else(
            || "请选择已连接的手机".to_owned(),
            |d| {
                format!(
                    "{}: {}% ({}/{})",
                    d.name,
                    u32::from(volume.saturating_sub(d.minimum)) * 100
                        / u32::from((d.maximum - d.minimum).max(1)),
                    volume,
                    d.maximum
                )
            },
        );
        let ready = !busy && device.is_some_and(|d| !d.fixed && d.speaker);
        let calibrated = !busy && !c.receivers.is_empty() && c.server.calibration.has_settings();
        let message = if busy || c.notice.is_empty() {
            calibration_status
        } else {
            c.notice.clone()
        };
        self.panel()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_lg()
                    .child(format!("接收设备（{}）", c.receivers.len())),
            )
            .child(list)
            .child(div().text_sm().child(label))
            .child(self.slider(
                "device-volume",
                SliderKind::Volume,
                volume,
                (minimum, maximum),
                can_adjust && device.is_some_and(|d| !d.fixed),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.button(
                        "volume-down",
                        "全部调低",
                        can_adjust && !c.receivers.is_empty(),
                        Command::AdjustAll(-1),
                        cx,
                    ))
                    .child(self.button(
                        "volume-up",
                        "全部调高",
                        can_adjust && !c.receivers.is_empty(),
                        Command::AdjustAll(1),
                        cx,
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .child(format!("群组音量：已保存校准值的 {}%", c.group)),
            )
            .child(self.slider(
                "group-volume",
                SliderKind::Group,
                c.group,
                (0, 100),
                calibrated,
                cx,
            ))
            .child(self.button(
                "apply-saved",
                "应用已保存的校准",
                calibrated,
                Command::Apply,
                cx,
            ))
            .child(
                div()
                    .text_sm()
                    .child(format!("校准音量上限：{}%", c.ceiling)),
            )
            .child(self.slider(
                "calibration-ceiling",
                SliderKind::Ceiling,
                c.ceiling,
                (1, 100),
                !busy,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.button("preview", "试听 5 秒", ready, Command::Preview, cx))
                    .child(self.button(
                        "calibrate",
                        "校准",
                        ready && c.receivers.len() >= 2,
                        Command::Calibrate,
                        cx,
                    ))
                    .child(self.button(
                        "cancel",
                        "取消",
                        busy && c.server.calibration.can_cancel(),
                        Command::Cancel,
                        cx,
                    )),
            )
            .child(div().text_sm().text_color(self.colors.muted).child(message))
    }

    fn information(&self, cx: &Context<Self>) -> Div {
        let info = &self.controller.server.info;
        let sample_type = match info.mix_sample_type.as_str() {
            "Float" => "浮点",
            "Int" => "整数",
            other => other,
        };
        let fingerprint = info
            .fingerprint
            .chars()
            .collect::<Vec<_>>()
            .chunks(48)
            .map(|chunk| chunk.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        self.panel()
            .flex_1()
            .min_w_0()
            .child(div().text_lg().child("服务信息"))
            .child(format!("输出设备：{}", info.device_name))
            .child(format!(
                "混音格式：{} Hz / {} 声道 / {} 位 {}",
                info.mix_sample_rate, info.mix_channels, info.mix_bits_per_sample, sample_type
            ))
            .child("传输格式：48000 Hz / 双声道 / i16 PCM")
            .child(format!("监听地址：{}", info.bind))
            .child("TLS 证书指纹")
            .child(div().font_family("Consolas").text_sm().child(fingerprint))
            .child(self.button(
                "copy-fingerprint",
                "复制指纹",
                true,
                Command::CopyFingerprint,
                cx,
            ))
            .child(format!("身份文件目录：{}", info.identity_dir.display()))
    }

    fn settings(&self) -> Div {
        let info = &self.controller.server.info;
        self.panel().flex_1().min_w_0()
            .child(div().text_lg().child("启动设置"))
            .child(format!("绑定地址：{}", info.bind))
            .child(format!("身份文件目录：{}", info.identity_dir.display()))
            .child(match &info.pairing {
                Ok(pairing) => format!("配对地址：{}", pairing.endpoint),
                Err(error) => format!("无法配对：{error}"),
            })
            .child("Soundwave 采集 Windows 默认输出设备的音频。")
            .child(div().text_sm().text_color(self.colors.muted).child("如需修改启动设置，请退出程序，再使用以下参数启动："))
            .child(div().font_family("Consolas").text_sm().child("audio-stream-server\n  --bind <地址>\n  --identity-dir <目录>\n  --pairing-host <IPv4>"))
            .child(div().text_sm().text_color(self.colors.muted).child("使用 --pairing-host 指定用于配对的局域网网卡地址。"))
    }
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.colors = Colors::for_window(window);
        let c = &self.controller;
        let enabled = c.server.transmission.is_enabled();
        let busy = c.server.calibration.status().0;
        let status = if busy {
            "正在播放校准音".to_owned()
        } else if !enabled {
            "音频已暂停".into()
        } else if c.receivers.is_empty() {
            "等待连接".into()
        } else {
            format!("正在向 {} 台设备传输", c.receivers.len())
        };
        let content = match c.page {
            Page::Receivers => self.receivers(cx),
            Page::Information => self.information(cx),
            Page::Settings => self.settings(),
        };
        div()
            .id("dashboard")
            .size_full()
            .bg(self.colors.background)
            .text_color(self.colors.text)
            .font_family("Microsoft YaHei UI")
            .text_sm()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = None),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_2xl().child("Soundwave"))
                            .child(div().text_color(self.colors.muted).child(status)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(self.button(
                                "toggle",
                                if enabled {
                                    "暂停音频"
                                } else {
                                    "启用音频"
                                },
                                !busy,
                                Command::Toggle,
                                cx,
                            ))
                            .child(self.button("exit", "退出", true, Command::Exit, cx)),
                    ),
            )
            .child(
                div().flex().gap_2().children(
                    [
                        ("receivers-tab", "接收设备", Page::Receivers),
                        ("info-tab", "服务信息", Page::Information),
                        ("settings-tab", "设置", Page::Settings),
                    ]
                    .into_iter()
                    .map(|(id, label, page)| {
                        self.button(id, label, true, Command::Show(page), cx)
                            .when(c.page == page, |el| el.border_color(self.colors.accent))
                    }),
                ),
            )
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_4()
                            .child(self.pairing(cx))
                            .child(content),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(self.colors.muted)
                    .child("关闭窗口后，Soundwave 将继续在系统托盘中运行。"),
            )
    }
}
