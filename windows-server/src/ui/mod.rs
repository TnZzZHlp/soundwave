use std::{
    cell::RefCell,
    ffi::c_void,
    mem::size_of,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    thread,
};

mod qr;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{network::server::AudioTransmissionGate, pairing::PairingInfo};
use qr::QrMatrix;
use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, COLOR_WINDOW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HBRUSH,
            HGDIOBJ, PAINTSTRUCT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
                NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu,
                CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
                GetClientRect, GetCursorPos, GetMessageW, HICON, HMENU, IDC_ARROW, IDI_APPLICATION,
                KillTimer, LoadCursorW, LoadIconW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
                MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, PostMessageW, PostQuitMessage,
                RegisterClassW, SW_HIDE, SW_RESTORE, SetForegroundWindow, SetTimer, SetWindowTextW,
                ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU,
                WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_PAINT, WM_RBUTTONUP, WM_TIMER,
                WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_MINIMIZEBOX, WS_SYSMENU,
                WS_TABSTOP, WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, w},
};

const TRAY_ICON_ID: u32 = 1;
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
const UPDATE_TIMER_ID: usize = 1;
const UPDATE_INTERVAL_MS: u32 = 250;
const COMMAND_SHOW_SERVER: u16 = 1;
const COMMAND_SERVER_INFORMATION: u16 = 2;
const COMMAND_SETTINGS: u16 = 3;
const COMMAND_TOGGLE_SERVICE: u16 = 4;
const COMMAND_EXIT: u16 = 5;

/// Immutable server details displayed by the Windows status UI.
#[derive(Clone)]
pub(crate) struct ServerUiInfo {
    pub(crate) device_name: String,
    pub(crate) mix_sample_rate: u32,
    pub(crate) mix_channels: u16,
    pub(crate) mix_bits_per_sample: u16,
    pub(crate) mix_sample_type: String,
    pub(crate) bind: SocketAddr,
    pub(crate) fingerprint: String,
    pub(crate) identity_dir: PathBuf,
    pub(crate) pairing: Result<PairingInfo, String>,
}

/// Owns the command channel used to keep the taskbar UI in sync with the server.
pub(crate) struct ServerUi {
    reporter: ServerUiReporter,
}

/// Sends live server status changes to the Windows UI thread.
#[derive(Clone)]
pub(crate) struct ServerUiReporter {
    sender: Sender<UiCommand>,
}

#[derive(Clone)]
enum UiStatus {
    WaitingForClient,
    Streaming(SocketAddr),
}

enum UiCommand {
    SetStatus(UiStatus),
    Shutdown,
}

struct UiContext {
    info: ServerUiInfo,
    status: UiStatus,
    shutdown: CancellationToken,
    transmission: AudioTransmissionGate,
    receiver: Receiver<UiCommand>,
    status_label: Option<HWND>,
    qr_matrix: Option<QrMatrix>,
}

#[derive(Clone, Copy)]
struct ControlBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

thread_local! {
    static UI_CONTEXT: RefCell<Option<UiContext>> = const { RefCell::new(None) };
}

impl ServerUi {
    /// Starts the taskbar window and notification-area icon on a dedicated UI thread.
    pub(crate) fn spawn(
        info: ServerUiInfo,
        shutdown: CancellationToken,
        transmission: AudioTransmissionGate,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);

        thread::Builder::new()
            .name("soundwave-windows-ui".to_owned())
            .spawn(move || run_ui_thread(info, shutdown, transmission, receiver, ready_sender))
            .context("could not start the Windows UI thread")?;

        match ready_receiver
            .recv()
            .context("Windows UI thread ended before it initialized")?
        {
            Ok(()) => Ok(Self {
                reporter: ServerUiReporter { sender },
            }),
            Err(error) => anyhow::bail!("could not initialize the Windows taskbar UI: {error}"),
        }
    }

    /// Returns a handle that stream workers can use to publish connection status.
    pub(crate) fn reporter(&self) -> ServerUiReporter {
        self.reporter.clone()
    }

    /// Closes the taskbar UI without delaying server shutdown.
    pub(crate) fn shutdown(&self) {
        let _ = self.reporter.sender.send(UiCommand::Shutdown);
    }
}

impl ServerUiReporter {
    /// Shows the server as ready for another Android client.
    pub(crate) fn waiting_for_client(&self) {
        let _ = self
            .sender
            .send(UiCommand::SetStatus(UiStatus::WaitingForClient));
    }

    /// Shows the currently connected Android peer.
    pub(crate) fn streaming_to(&self, peer: SocketAddr) {
        let _ = self
            .sender
            .send(UiCommand::SetStatus(UiStatus::Streaming(peer)));
    }
}

/// Shows a visible error when a GUI-subsystem server cannot start.
pub(crate) fn show_startup_error(error: &str) {
    show_message(
        "Soundwave Server",
        &format!("Soundwave Server could not start.\r\n\r\n{error}"),
        true,
    );
}

/// Confirms completion of the capture-only debug path without requiring a console.
pub(crate) fn show_capture_complete(path: &Path) {
    show_message(
        "Soundwave Server",
        &format!(
            "Wrote 48 kHz stereo i16 little-endian PCM to:\r\n{}",
            path.display()
        ),
        false,
    );
}

fn show_message(title: &str, text: &str, error: bool) {
    let title = HSTRING::from(title);
    let text = HSTRING::from(text);
    let style = if error {
        MB_OK | MB_ICONERROR
    } else {
        MB_OK | MB_ICONINFORMATION
    };
    let _ = unsafe { MessageBoxW(None, &text, &title, style) };
}

impl UiStatus {
    fn label(&self, transmission_enabled: bool) -> String {
        match (self, transmission_enabled) {
            (Self::WaitingForClient, true) => "Status: Waiting for Android client".to_owned(),
            (Self::WaitingForClient, false) => "Status: Disabled; no audio will be sent".to_owned(),
            (Self::Streaming(peer), true) => format!("Status: Streaming to {peer}"),
            (Self::Streaming(peer), false) => {
                format!("Status: Disabled; audio paused for {peer}")
            }
        }
    }
}

fn run_ui_thread(
    info: ServerUiInfo,
    shutdown: CancellationToken,
    transmission: AudioTransmissionGate,
    receiver: Receiver<UiCommand>,
    ready_sender: SyncSender<Result<(), String>>,
) {
    UI_CONTEXT.with(|context| {
        *context.borrow_mut() = Some(UiContext {
            info,
            status: UiStatus::WaitingForClient,
            shutdown,
            transmission,
            receiver,
            status_label: None,
            qr_matrix: None,
        });
    });

    match initialize_main_window() {
        Ok(hwnd) => {
            let _ = ready_sender.send(Ok(()));
            if let Err(error) = run_message_loop() {
                warn!(%error, "Windows UI message loop ended with an error");
            }
            remove_tray_icon(hwnd);
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        Err(error) => {
            let _ = ready_sender.send(Err(error.to_string()));
        }
    }

    UI_CONTEXT.with(|context| {
        context.borrow_mut().take();
    });
}

fn initialize_main_window() -> Result<HWND> {
    prepare_pairing_qr();

    let module: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .context("could not obtain the application module handle")?
        .into();
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }
        .context("could not load the Windows application icon")?;
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .context("could not load the Windows arrow cursor")?;
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: module,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: system_color_brush(COLOR_WINDOW.0),
        lpszClassName: w!("SoundwaveServerWindow"),
        ..Default::default()
    };

    if unsafe { RegisterClassW(&class) } == 0 {
        anyhow::bail!("could not register the Soundwave taskbar window class");
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("SoundwaveServerWindow"),
            w!("Soundwave Server"),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            840,
            590,
            None,
            None,
            Some(module),
            None,
        )
    }
    .context("could not create the Soundwave taskbar window")?;

    if let Err(error) = add_tray_icon(hwnd, icon) {
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }

    if unsafe { SetTimer(Some(hwnd), UPDATE_TIMER_ID, UPDATE_INTERVAL_MS, None) } == 0 {
        remove_tray_icon(hwnd);
        let _ = unsafe { DestroyWindow(hwnd) };
        anyhow::bail!("could not create the taskbar UI update timer");
    }

    Ok(hwnd)
}

fn system_color_brush(color: i32) -> HBRUSH {
    HBRUSH((color + 1) as usize as *mut c_void)
}

fn prepare_pairing_qr() {
    let payload = UI_CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .and_then(|context| context.info.pairing.as_ref().ok())
            .map(|pairing| pairing.uri.clone())
    });
    let Some(payload) = payload else {
        return;
    };

    match QrMatrix::encode(&payload) {
        Ok(matrix) => UI_CONTEXT.with(|context| {
            if let Some(context) = context.borrow_mut().as_mut() {
                context.qr_matrix = Some(matrix);
            }
        }),
        Err(error) => UI_CONTEXT.with(|context| {
            if let Some(context) = context.borrow_mut().as_mut() {
                context.info.pairing = Err(error);
                context.qr_matrix = None;
            }
        }),
    }
}

fn run_message_loop() -> Result<()> {
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        match result.0 {
            -1 => anyhow::bail!("the Windows UI message loop failed"),
            0 => return Ok(()),
            _ => {
                let _ = unsafe { TranslateMessage(&message) };
                let _ = unsafe { DispatchMessageW(&message) };
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            create_dashboard_controls(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_command(hwnd, (wparam.0 & 0xffff) as u16);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = unsafe { KillTimer(Some(hwnd), UPDATE_TIMER_ID) };
            remove_tray_icon(hwnd);
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_PAINT => {
            paint_pairing_qr(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            process_ui_commands(hwnd);
            LRESULT(0)
        }
        TRAY_CALLBACK_MESSAGE => {
            match (lparam.0 as u32) & 0xffff {
                WM_CONTEXTMENU | WM_RBUTTONUP => show_context_menu(hwnd),
                WM_LBUTTONUP | NIN_SELECT => show_dashboard(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn paint_pairing_qr(hwnd: HWND) {
    let matrix = UI_CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .and_then(|context| context.qr_matrix.clone())
    });
    let mut paint = PAINTSTRUCT::default();
    let device_context = unsafe { BeginPaint(hwnd, &mut paint) };
    if let Some(matrix) = matrix {
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
            render_qr_matrix(device_context, client, &matrix);
        }
    }
    let _ = unsafe { EndPaint(hwnd, &paint) };
}

fn render_qr_matrix(
    device_context: windows::Win32::Graphics::Gdi::HDC,
    client: RECT,
    matrix: &QrMatrix,
) {
    const PANEL_LEFT: i32 = 410;
    const PANEL_TOP: i32 = 95;
    const PANEL_SIZE: i32 = 380;
    const QUIET_ZONE_MODULES: i32 = 4;

    let panel = RECT {
        left: PANEL_LEFT,
        top: PANEL_TOP,
        right: (PANEL_LEFT + PANEL_SIZE).min(client.right),
        bottom: (PANEL_TOP + PANEL_SIZE).min(client.bottom),
    };
    if panel.right <= panel.left || panel.bottom <= panel.top {
        return;
    }

    let white_brush = unsafe { CreateSolidBrush(COLORREF(0x00FF_FFFF)) };
    let black_brush = unsafe { CreateSolidBrush(COLORREF(0x0000_0000)) };
    if white_brush.is_invalid() || black_brush.is_invalid() {
        if !white_brush.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(white_brush.0)) };
        }
        if !black_brush.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(black_brush.0)) };
        }
        return;
    }

    let _ = unsafe { FillRect(device_context, &panel, white_brush) };
    let total_modules = matrix.size() + QUIET_ZONE_MODULES * 2;
    let module_size =
        ((panel.right - panel.left).min(panel.bottom - panel.top) / total_modules).max(1);
    let rendered_size = module_size * total_modules;
    let origin_x = panel.left + (panel.right - panel.left - rendered_size) / 2;
    let origin_y = panel.top + (panel.bottom - panel.top - rendered_size) / 2;

    for y in 0..matrix.size() {
        for x in 0..matrix.size() {
            if !matrix.is_dark(x, y) {
                continue;
            }
            let left = origin_x + (x + QUIET_ZONE_MODULES) * module_size;
            let top = origin_y + (y + QUIET_ZONE_MODULES) * module_size;
            let module = RECT {
                left,
                top,
                right: left + module_size,
                bottom: top + module_size,
            };
            let _ = unsafe { FillRect(device_context, &module, black_brush) };
        }
    }

    let _ = unsafe { DeleteObject(HGDIOBJ(white_brush.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(black_brush.0)) };
}

fn process_ui_commands(hwnd: HWND) {
    let mut updated_status = None;
    let mut close = false;

    UI_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let Some(context) = context.as_mut() else {
            return;
        };

        loop {
            match context.receiver.try_recv() {
                Ok(UiCommand::SetStatus(status)) => {
                    context.status = status;
                    updated_status = context.status_label.map(|label| {
                        (
                            label,
                            context.status.label(context.transmission.is_enabled()),
                        )
                    });
                }
                Ok(UiCommand::Shutdown) | Err(TryRecvError::Disconnected) => {
                    close = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        if context.shutdown.is_cancelled() {
            close = true;
        }
    });

    if let Some((label, text)) = updated_status {
        let text = HSTRING::from(text);
        let _ = unsafe { SetWindowTextW(label, &text) };
    }

    if close {
        request_shutdown(hwnd);
    }
}

fn request_shutdown(hwnd: HWND) {
    UI_CONTEXT.with(|context| {
        if let Some(context) = context.borrow().as_ref() {
            context.shutdown.cancel();
        }
    });
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn handle_command(hwnd: HWND, command: u16) {
    match command {
        COMMAND_SHOW_SERVER => show_dashboard(hwnd),
        COMMAND_SERVER_INFORMATION => show_server_information(hwnd),
        COMMAND_SETTINGS => show_settings(hwnd),
        COMMAND_TOGGLE_SERVICE => toggle_audio_transmission(),
        COMMAND_EXIT => request_shutdown(hwnd),
        _ => {}
    }
}

fn toggle_audio_transmission() {
    let Some((enabled, status_label)) = UI_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let context = context.as_mut()?;
        let enabled = !context.transmission.is_enabled();
        context.transmission.set_enabled(enabled);
        let status_label = context
            .status_label
            .map(|label| (label, context.status.label(enabled)));
        Some((enabled, status_label))
    }) else {
        return;
    };

    if let Some((label, text)) = status_label {
        let text = HSTRING::from(text);
        let _ = unsafe { SetWindowTextW(label, &text) };
    }
    info!(enabled, "audio transmission service toggled");
}

fn audio_transmission_is_enabled() -> bool {
    UI_CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .is_none_or(|context| context.transmission.is_enabled())
    })
}

fn show_dashboard(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    let _ = unsafe { SetForegroundWindow(hwnd) };
}

fn show_context_menu(hwnd: HWND) {
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(error) => {
            warn!(%error, "could not create the Soundwave notification-area menu");
            return;
        }
    };

    let toggle_label = if audio_transmission_is_enabled() {
        w!("Disable service")
    } else {
        w!("Enable service")
    };
    let menu_result = unsafe {
        AppendMenuW(
            menu,
            MF_STRING,
            COMMAND_SHOW_SERVER as usize,
            w!("Show server"),
        )
        .and_then(|()| {
            AppendMenuW(
                menu,
                MF_STRING,
                COMMAND_TOGGLE_SERVICE as usize,
                toggle_label,
            )
        })
        .and_then(|()| AppendMenuW(menu, MF_SEPARATOR, 0, w!("")))
        .and_then(|()| {
            AppendMenuW(
                menu,
                MF_STRING,
                COMMAND_SERVER_INFORMATION as usize,
                w!("Server information"),
            )
        })
        .and_then(|()| AppendMenuW(menu, MF_STRING, COMMAND_SETTINGS as usize, w!("Settings")))
        .and_then(|()| AppendMenuW(menu, MF_SEPARATOR, 0, w!("")))
        .and_then(|()| AppendMenuW(menu, MF_STRING, COMMAND_EXIT as usize, w!("Exit")))
    };
    if let Err(error) = menu_result {
        let _ = unsafe { DestroyMenu(menu) };
        warn!(%error, "could not populate the Soundwave notification-area menu");
        return;
    }

    let _ = unsafe { SetForegroundWindow(hwnd) };
    let mut point = POINT::default();
    if let Err(error) = unsafe { GetCursorPos(&mut point) } {
        let _ = unsafe { DestroyMenu(menu) };
        warn!(%error, "could not locate the cursor for the Soundwave notification-area menu");
        return;
    }

    let command = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
    }
    .0 as u16;
    let _ = unsafe { DestroyMenu(menu) };
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };

    if command != 0 {
        handle_command(hwnd, command);
    }
}

fn show_server_information(hwnd: HWND) {
    let Some(text) = UI_CONTEXT.with(|context| {
        context.borrow().as_ref().map(|context| {
            let pairing = match &context.info.pairing {
                Ok(pairing) => format!("Pairing QR endpoint:\r\n{}", pairing.endpoint),
                Err(error) => format!("Pairing QR unavailable:\r\n{error}"),
            };
            format!(
                "{}\r\n\r\nAudio device:\r\n{}\r\nMix format: {} Hz / {} channels / {} bit {}\r\nCapture format: 48000 Hz / Stereo / i16\r\n\r\nListening address:\r\n{}\r\n\r\n{}\r\n\r\nTLS certificate fingerprint:\r\n{}\r\n\r\nIdentity directory:\r\n{}\r\n\r\nScan the pairing QR code in Audio Stream, then review the fields before connecting.",
                context.status.label(context.transmission.is_enabled()),
                context.info.device_name,
                context.info.mix_sample_rate,
                context.info.mix_channels,
                context.info.mix_bits_per_sample,
                context.info.mix_sample_type,
                context.info.bind,
                pairing,
                context.info.fingerprint,
                context.info.identity_dir.display(),
            )
        })
    }) else {
        return;
    };

    let text = HSTRING::from(text);
    let _ = unsafe {
        MessageBoxW(
            Some(hwnd),
            &text,
            w!("Soundwave Server information"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

fn show_settings(hwnd: HWND) {
    let Some(text) = UI_CONTEXT.with(|context| {
        context.borrow().as_ref().map(|context| {
            let pairing = match &context.info.pairing {
                Ok(pairing) => format!("Pairing QR endpoint: {}", pairing.endpoint),
                Err(error) => format!("Pairing QR unavailable: {error}"),
            };
            format!(
                "Current startup settings\r\n\r\nBind address: {}\r\nIdentity directory: {}\r\n{}\r\n\r\nThese settings are applied when the server starts. To change them, exit Soundwave and start it again with:\r\n\r\naudio-stream-server --bind <ADDRESS> --identity-dir <DIRECTORY> --pairing-host <IPv4>\r\n\r\nUse --pairing-host when the QR code must advertise a specific LAN adapter. Soundwave V0.1 always captures the default Windows output device.",
                context.info.bind,
                context.info.identity_dir.display(),
                pairing,
            )
        })
    }) else {
        return;
    };

    let text = HSTRING::from(text);
    let _ = unsafe { MessageBoxW(Some(hwnd), &text, w!("Soundwave Server settings"), MB_OK) };
}

fn add_tray_icon(hwnd: HWND, icon: HICON) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_SHOWTIP | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide_string(&mut data.szTip, "Soundwave Server");

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        anyhow::bail!("could not add the Soundwave notification-area icon");
    }

    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
        warn!("could not enable modern notification-area icon behavior");
    }

    Ok(())
}

fn remove_tray_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn copy_wide_string(destination: &mut [u16], source: &str) {
    for (slot, code_unit) in destination.iter_mut().zip(source.encode_utf16()) {
        *slot = code_unit;
    }
}

fn create_dashboard_controls(hwnd: HWND) {
    let Some((device_name, bind, status, pairing)) = UI_CONTEXT.with(|context| {
        context.borrow().as_ref().map(|context| {
            let pairing = match &context.info.pairing {
                Ok(pairing) => format!(
                    "Scan in Audio Stream\r\nPairing endpoint: {}",
                    pairing.endpoint
                ),
                Err(error) => format!("QR pairing unavailable\r\n{error}"),
            };
            (
                context.info.device_name.clone(),
                context.info.bind,
                context.status.label(context.transmission.is_enabled()),
                pairing,
            )
        })
    }) else {
        return;
    };

    let _ = create_static(hwnd, "Soundwave Server", 20, 18, 360, 24);
    let status_label = create_static(hwnd, &status, 20, 55, 360, 24);
    let details = format!(
        "Listening: {bind}\r\nDefault output: {device_name}\r\nThe QR code contains the LAN endpoint and pinned TLS fingerprint."
    );
    let _ = create_static(hwnd, &details, 20, 92, 360, 90);
    let _ = create_button(
        hwnd,
        "Server information",
        COMMAND_SERVER_INFORMATION,
        20,
        220,
        155,
        30,
    );
    let _ = create_button(hwnd, "Settings", COMMAND_SETTINGS, 190, 220, 110, 30);
    let _ = create_button(hwnd, "Exit", COMMAND_EXIT, 315, 220, 70, 30);
    let _ = create_static(hwnd, &pairing, 410, 18, 380, 65);

    UI_CONTEXT.with(|context| {
        if let Some(context) = context.borrow_mut().as_mut() {
            context.status_label = status_label;
        }
    });
}

fn create_static(hwnd: HWND, text: &str, x: i32, y: i32, width: i32, height: i32) -> Option<HWND> {
    create_control(
        hwnd,
        w!("STATIC"),
        text,
        WINDOW_STYLE(0),
        None,
        ControlBounds {
            x,
            y,
            width,
            height,
        },
    )
}

fn create_button(
    hwnd: HWND,
    text: &str,
    command: u16,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<HWND> {
    create_control(
        hwnd,
        w!("BUTTON"),
        text,
        WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
        Some(HMENU(command as usize as *mut c_void)),
        ControlBounds {
            x,
            y,
            width,
            height,
        },
    )
}

fn create_control(
    hwnd: HWND,
    class_name: windows::core::PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    menu: Option<HMENU>,
    bounds: ControlBounds,
) -> Option<HWND> {
    let text = HSTRING::from(text);
    match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            &text,
            WS_CHILD | WS_VISIBLE | style,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            Some(hwnd),
            menu,
            None,
            None,
        )
    } {
        Ok(control) => Some(control),
        Err(error) => {
            warn!(%error, "could not create a Soundwave taskbar window control");
            None
        }
    }
}
