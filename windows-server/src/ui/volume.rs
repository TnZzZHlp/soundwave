use std::{
    cell::RefCell,
    mem::size_of,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Gdi::COLOR_WINDOW,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{
                ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TBM_SETPOS,
                TBM_SETRANGEMAX, TBM_SETRANGEMIN,
            },
            Input::KeyboardAndMouse::{EnableWindow, GetCapture},
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, PCWSTR, w},
};

use crate::{
    application::{
        calibration::{CalibrationController, CalibrationRequest},
        registry::ReceiverRegistry,
    },
    audio::transmission::AudioTransmissionGate,
};

const TBM_GETPOS: u32 = WM_USER;
const LIST: u16 = 101;
const PREVIEW: u16 = 102;
const CALIBRATE: u16 = 103;
const CANCEL: u16 = 104;
const APPLY: u16 = 105;

struct PendingVolume {
    id: u64,
    index: u16,
    changed: Instant,
}

struct Window {
    hwnd: HWND,
    list: HWND,
    device_label: HWND,
    device_slider: HWND,
    group_label: HWND,
    group_slider: HWND,
    ceiling_label: HWND,
    ceiling_slider: HWND,
    status: HWND,
    preview: HWND,
    calibrate: HWND,
    cancel: HWND,
    apply: HWND,
    ids: Vec<u64>,
    rows: Vec<String>,
    selected: Option<u64>,
    pending: Option<PendingVolume>,
    group_pending: Option<Instant>,
    replies: Vec<(u64, u32, Instant)>,
    notice: String,
    last_status: String,
    registry: ReceiverRegistry,
    transmission: AudioTransmissionGate,
    shutdown: CancellationToken,
    controller: CalibrationController,
    profile_path: PathBuf,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

pub(super) fn show(parent: HWND) {
    let existing = WINDOW.with(|window| window.borrow().as_ref().map(|w| w.hwnd));
    if let Some(hwnd) = existing {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        let _ = unsafe { SetForegroundWindow(hwnd) };
        return;
    }
    let context = super::UI_CONTEXT.with(|context| {
        context.borrow().as_ref().map(|context| {
            (
                context.registry.clone(),
                context.transmission.clone(),
                context.shutdown.clone(),
                context.calibration.clone(),
                context.info.identity_dir.join("volume-calibration.txt"),
            )
        })
    });
    let Some((registry, transmission, shutdown, controller, profile_path)) = context else {
        return;
    };
    match create(
        parent,
        registry,
        transmission,
        shutdown,
        controller,
        profile_path,
    ) {
        Ok(window) => {
            let hwnd = window.hwnd;
            WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
            refresh();
            let _ = unsafe { SetTimer(Some(hwnd), 1, 150, None) };
            let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
            let _ = unsafe { SetForegroundWindow(hwnd) };
        }
        Err(error) => super::show_message("Device volume", &error.to_string(), true),
    }
}

fn create(
    parent: HWND,
    registry: ReceiverRegistry,
    transmission: AudioTransmissionGate,
    shutdown: CancellationToken,
    controller: CalibrationController,
    profile_path: PathBuf,
) -> Result<Window> {
    let module: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES,
    };
    unsafe { InitCommonControlsEx(&controls) }.ok()?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        hbrBackground: super::system_color_brush(COLOR_WINDOW.0),
        lpszClassName: w!("SoundwaveVolumeWindow"),
        ..Default::default()
    };
    unsafe { RegisterClassW(&class) };
    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("SoundwaveVolumeWindow"),
            w!("Device volume"),
            WS_CAPTION | WS_SYSMENU,
            point
                .x
                .max(0)
                .min(GetSystemMetrics(SM_CXSCREEN).saturating_sub(660).max(0)),
            point
                .y
                .max(0)
                .min(GetSystemMetrics(SM_CYSCREEN).saturating_sub(720).max(0)),
            650,
            710,
            Some(parent),
            None,
            Some(module),
            None,
        )
    }
    .context("Could not create volume window")?;
    let build = || -> Result<Window> {
        let list = control(
            hwnd,
            w!("LISTBOX"),
            "",
            LIST,
            [16, 16, 600, 148],
            LBS_NOTIFY as u32 | WS_VSCROLL.0 | WS_BORDER.0,
        )?;
        let device_label = control(
            hwnd,
            w!("STATIC"),
            "Select a connected phone",
            0,
            [16, 174, 600, 24],
            0,
        )?;
        let device_slider = slider(hwnd, [16, 199, 600, 32])?;
        let group_label = control(
            hwnd,
            w!("STATIC"),
            "Group: 100% of saved calibration",
            0,
            [16, 240, 600, 24],
            0,
        )?;
        let group_slider = slider(hwnd, [16, 265, 600, 32])?;
        position(group_slider, 100);
        let apply = control(
            hwnd,
            w!("BUTTON"),
            "Apply saved calibration",
            APPLY,
            [16, 304, 240, 30],
            BS_PUSHBUTTON as u32,
        )?;
        let ceiling_label = control(
            hwnd,
            w!("STATIC"),
            "Calibration ceiling: 75% of each phone's system range",
            0,
            [16, 350, 600, 24],
            0,
        )?;
        let ceiling_slider = slider(hwnd, [16, 375, 600, 32])?;
        position(ceiling_slider, 75);
        let preview = control(
            hwnd,
            w!("BUTTON"),
            "Preview selected phone (5s)",
            PREVIEW,
            [16, 416, 250, 32],
            BS_PUSHBUTTON as u32,
        )?;
        let calibrate = control(
            hwnd,
            w!("BUTTON"),
            "Match to selected phone",
            CALIBRATE,
            [276, 416, 220, 32],
            BS_PUSHBUTTON as u32,
        )?;
        let cancel = control(
            hwnd,
            w!("BUTTON"),
            "Stop",
            CANCEL,
            [506, 416, 110, 32],
            BS_PUSHBUTTON as u32,
        )?;
        control(
            hwnd,
            w!("STATIC"),
            "Place the Windows default microphone at your listening position.\r\nSilence PC speakers/other audio. Disable microphone AGC/enhancements.\r\nMatching is relative, not calibrated SPL; other volume levels may differ.",
            0,
            [16, 462, 600, 62],
            0,
        )?;
        let status = control(
            hwnd,
            w!("EDIT"),
            "",
            0,
            [16, 538, 600, 112],
            ES_MULTILINE as u32 | ES_READONLY as u32 | WS_VSCROLL.0 | WS_BORDER.0,
        )?;
        Ok(Window {
            hwnd,
            list,
            device_label,
            device_slider,
            group_label,
            group_slider,
            ceiling_label,
            ceiling_slider,
            status,
            preview,
            calibrate,
            cancel,
            apply,
            ids: Vec::new(),
            rows: Vec::new(),
            selected: None,
            pending: None,
            group_pending: None,
            replies: Vec::new(),
            notice: String::new(),
            last_status: String::new(),
            registry,
            transmission,
            shutdown,
            controller,
            profile_path,
        })
    };
    let result = build();
    if result.is_err() {
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    result
}

fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    id: u16,
    bounds: [i32; 4],
    style: u32,
) -> Result<HWND> {
    let [x, y, width, height] = bounds;
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style),
            x,
            y,
            width,
            height,
            Some(parent),
            Some(HMENU(usize::from(id) as *mut _)),
            None,
            None,
        )
    }
    .context("Could not create volume control")
}

fn slider(parent: HWND, bounds: [i32; 4]) -> Result<HWND> {
    let hwnd = control(parent, w!("msctls_trackbar32"), "", 0, bounds, 0)?;
    range(hwnd, 0, 100);
    Ok(hwnd)
}

fn send(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe { SendMessageW(hwnd, message, Some(WPARAM(wparam)), Some(LPARAM(lparam))) }.0
}

fn position(hwnd: HWND, index: u16) {
    send(hwnd, TBM_SETPOS, 1, index as isize);
}
fn value(hwnd: HWND) -> u16 {
    send(hwnd, TBM_GETPOS, 0, 0).clamp(0, 512) as u16
}
fn range(hwnd: HWND, minimum: u16, maximum: u16) {
    send(hwnd, TBM_SETRANGEMIN, 0, minimum as isize);
    send(hwnd, TBM_SETRANGEMAX, 1, maximum as isize);
}
fn label(hwnd: HWND, text: &str) {
    let _ = unsafe { SetWindowTextW(hwnd, &HSTRING::from(text)) };
}
fn enable(hwnd: HWND, enabled: bool) {
    let _ = unsafe { EnableWindow(hwnd, enabled) };
}

fn refresh() {
    WINDOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(window) = slot.as_mut() else {
            return;
        };
        let snapshots = window.registry.snapshots();
        let (busy, calibration_status) = window.controller.status();
        let can_adjust = window.controller.can_adjust();
        let rows: Vec<String> = snapshots
            .iter()
            .map(|r| match &r.device {
                Some(d) => format!(
                    "{} [{}]  {}/{}{}{}",
                    d.name,
                    r.peer,
                    d.volume,
                    d.maximum,
                    if d.fixed { " (fixed)" } else { "" },
                    if d.failed { " (request rejected)" } else { "" }
                ),
                None => format!("{}  waiting for device status...", r.peer),
            })
            .collect();
        if rows != window.rows {
            send(window.list, LB_RESETCONTENT, 0, 0);
            window.ids = snapshots.iter().map(|r| r.id).collect();
            for row in &rows {
                let text = HSTRING::from(row);
                send(window.list, LB_ADDSTRING, 0, text.as_ptr() as isize);
            }
            if !window.ids.iter().any(|id| Some(*id) == window.selected) {
                window.selected = window.ids.first().copied();
            }
            if let Some(index) = window
                .ids
                .iter()
                .position(|id| Some(*id) == window.selected)
            {
                send(window.list, LB_SETCURSEL, index, 0);
            }
            window.rows = rows;
        }
        window.replies.retain(|&(id, request_id, at)| {
            if let Some(reply) = window.registry.take_reply(id, request_id) {
                if reply.failed {
                    window.notice = format!(
                        "{} rejected the volume request; check system policy or safe-volume limits",
                        reply.name
                    );
                }
                return false;
            }
            if at.elapsed() > Duration::from_secs(4) {
                window.notice =
                    "No acknowledgement for a volume change; check the device connection".into();
                return false;
            }
            true
        });
        if let Some(pending) = window
            .pending
            .as_ref()
            .filter(|p| p.changed.elapsed() >= Duration::from_millis(120))
        {
            if can_adjust {
                match window.registry.request(pending.id, Some(pending.index)) {
                    Ok(request_id) => window
                        .replies
                        .push((pending.id, request_id, Instant::now())),
                    Err(error) => window.notice = error.to_string(),
                }
            }
            window.pending = None;
        }
        if window
            .group_pending
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(120))
        {
            match window
                .controller
                .apply_group(&window.registry, value(window.group_slider))
            {
                Ok(requests) => window.replies.extend(
                    requests
                        .into_iter()
                        .map(|(id, request_id)| (id, request_id, Instant::now())),
                ),
                Err(error) => window.notice = error.to_string(),
            }
            window.group_pending = None;
        }
        let selected = snapshots
            .iter()
            .find(|r| Some(r.id) == window.selected)
            .and_then(|r| r.device.as_ref());
        if let Some(device) = selected {
            label(
                window.device_label,
                &format!(
                    "{}: {}% ({}/{}){}",
                    device.name,
                    u32::from(device.volume - device.minimum) * 100
                        / u32::from((device.maximum - device.minimum).max(1)),
                    device.volume,
                    device.maximum,
                    if device.fixed { " - fixed volume" } else { "" }
                ),
            );
            if unsafe { GetCapture() } != window.device_slider
                && window.pending.is_none()
                && !window
                    .replies
                    .iter()
                    .any(|(id, _, _)| Some(*id) == window.selected)
            {
                range(window.device_slider, device.minimum, device.maximum);
                position(window.device_slider, device.volume);
            }
        } else {
            label(window.device_label, "No device status available");
        }
        enable(
            window.device_slider,
            can_adjust && selected.is_some_and(|d| !d.fixed),
        );
        enable(window.list, !busy);
        enable(
            window.group_slider,
            !busy && !snapshots.is_empty() && window.controller.has_settings(),
        );
        enable(window.ceiling_slider, !busy);
        enable(
            window.preview,
            !busy && selected.is_some_and(|d| !d.fixed && d.speaker),
        );
        enable(
            window.calibrate,
            !busy && snapshots.len() >= 2 && selected.is_some(),
        );
        enable(window.cancel, busy && window.controller.can_cancel());
        enable(
            window.apply,
            !busy && !snapshots.is_empty() && window.controller.has_settings(),
        );
        let text = if busy || window.notice.is_empty() {
            calibration_status
        } else {
            window.notice.clone()
        };
        if text != window.last_status {
            label(window.status, &text);
            window.last_status = text;
        }
    });
}

fn command(command: u16, notification: u16) {
    if command == CALIBRATE {
        let confirmed = unsafe {
            MessageBoxW(
                None,
                w!(
                    "Pause streaming and match all phone speakers to the selected phone?\n\nFirst preview the reference at a comfortable level. Other phones start low and rise only to the calibration ceiling. Keep the microphone at the listening position with AGC/enhancements disabled. Disconnect headphones and external audio outputs.\n\nSystem media volume changes affect other apps too."
                ),
                w!("Calibrate relative loudness"),
                MB_OKCANCEL | MB_ICONWARNING,
            )
        };
        if confirmed != IDOK {
            return;
        }
    }
    WINDOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(window) = slot.as_mut() else {
            return;
        };
        match command {
            LIST if notification == LBN_SELCHANGE as u16 => {
                if let Some(pending) = window.pending.take() {
                    match window.registry.request(pending.id, Some(pending.index)) {
                        Ok(request) => window.replies.push((pending.id, request, Instant::now())),
                        Err(error) => window.notice = error.to_string(),
                    }
                }
                let index = send(window.list, LB_GETCURSEL, 0, 0);
                window.selected = usize::try_from(index)
                    .ok()
                    .and_then(|i| window.ids.get(i).copied());
                if let Some(device) = window
                    .registry
                    .snapshots()
                    .iter()
                    .find(|r| Some(r.id) == window.selected)
                    .and_then(|r| r.device.as_ref())
                {
                    range(window.device_slider, device.minimum, device.maximum);
                    position(window.device_slider, device.volume);
                }
            }
            CANCEL => window.controller.cancel(),
            APPLY => {
                window.notice.clear();
                position(window.group_slider, 100);
                label(window.group_label, "Group: 100% of saved calibration");
                match window.controller.apply_saved(&window.registry) {
                    Ok(requests) => window.replies.extend(
                        requests
                            .into_iter()
                            .map(|(id, request_id)| (id, request_id, Instant::now())),
                    ),
                    Err(error) => window.notice = error.to_string(),
                }
            }
            PREVIEW | CALIBRATE => {
                window.notice.clear();
                if let Some(pending) = window.pending.take() {
                    match window.registry.request(pending.id, Some(pending.index)) {
                        Ok(request_id) => {
                            window
                                .replies
                                .push((pending.id, request_id, Instant::now()))
                        }
                        Err(error) => {
                            window.notice = error.to_string();
                            return;
                        }
                    }
                }
                if window.group_pending.take().is_some() {
                    match window
                        .controller
                        .apply_group(&window.registry, value(window.group_slider))
                    {
                        Ok(requests) => window.replies.extend(
                            requests
                                .into_iter()
                                .map(|(id, request)| (id, request, Instant::now())),
                        ),
                        Err(error) => {
                            window.notice = error.to_string();
                            return;
                        }
                    }
                }
                if let Some(reference) = window.selected {
                    let request = CalibrationRequest {
                        reference,
                        ceiling_percent: value(window.ceiling_slider).max(1),
                        preview: command == PREVIEW,
                        registry: window.registry.clone(),
                        transmission: window.transmission.clone(),
                        shutdown: window.shutdown.clone(),
                        profile_path: window.profile_path.clone(),
                    };
                    if let Err(error) = window.controller.start(request) {
                        window.notice = error.to_string();
                    }
                }
            }
            _ => {}
        }
    });
    refresh();
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER => {
            refresh();
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as u16;
            let notification = (wparam.0 >> 16) as u16;
            if (id == LIST && notification == LBN_SELCHANGE as u16)
                || (matches!(id, PREVIEW | CALIBRATE | CANCEL | APPLY)
                    && notification == BN_CLICKED as u16)
            {
                command(id, notification);
            }
            LRESULT(0)
        }
        WM_HSCROLL => {
            WINDOW.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(window) = slot.as_mut() else {
                    return;
                };
                let control = HWND(lparam.0 as *mut _);
                if control == window.device_slider {
                    if let Some(id) = window.selected {
                        window.notice.clear();
                        window.pending = Some(PendingVolume {
                            id,
                            index: value(control),
                            changed: Instant::now(),
                        });
                    }
                } else if control == window.group_slider {
                    window.notice.clear();
                    label(
                        window.group_label,
                        &format!("Group: {}% of saved calibration", value(control)),
                    );
                    window.group_pending = Some(Instant::now());
                } else if control == window.ceiling_slider {
                    label(
                        window.ceiling_label,
                        &format!(
                            "Calibration ceiling: {}% of each phone's system range",
                            value(control).max(1)
                        ),
                    );
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            WINDOW.with(|slot| {
                if let Some(window) = slot.borrow().as_ref() {
                    window.controller.cancel();
                }
            });
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = unsafe { KillTimer(Some(hwnd), 1) };
            WINDOW.with(|slot| {
                slot.borrow_mut().take();
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
