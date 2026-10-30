use std::{cell::RefCell, mem::size_of, sync::mpsc::SyncSender};

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
                NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, GetCursorPos, IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING,
                PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
                TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE, WM_APP,
                WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
            },
        },
    },
    core::{HSTRING, w},
};

use super::model::{Command, Page};
use crate::audio::transmission::AudioTransmissionGate;

const CALLBACK: u32 = WM_APP + 1;

struct State {
    sender: SyncSender<Command>,
    transmission: AudioTransmissionGate,
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub(super) struct Tray {
    hwnd: HWND,
}

impl Tray {
    pub fn new(sender: SyncSender<Command>, transmission: AudioTransmissionGate) -> Result<Self> {
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("SoundwaveGpuiTray"),
            ..Default::default()
        };
        anyhow::ensure!(unsafe { RegisterClassW(&class) } != 0, "无法注册托盘窗口");
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SoundwaveGpuiTray"),
                w!("Soundwave"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .context("无法创建托盘窗口")?;
        let tray = Self { hwnd };
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                sender,
                transmission,
                taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            })
        });
        tray.add_icon()?;
        Ok(tray)
    }

    fn add_icon(&self) -> Result<()> {
        add_icon(self.hwnd)
    }
}

fn add_icon(hwnd: HWND) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_SHOWTIP | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }?,
        ..Default::default()
    };
    for (slot, unit) in data.szTip.iter_mut().zip("Soundwave 服务端".encode_utf16()) {
        *slot = unit;
    }
    anyhow::ensure!(
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool(),
        "无法添加托盘图标"
    );
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
    Ok(())
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        let _ = unsafe { DestroyWindow(self.hwnd) };
        STATE.with(|state| {
            state.borrow_mut().take();
        });
    }
}

fn send(command: Command) {
    STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            let _ = state.sender.try_send(command);
        }
    });
}

fn menu(hwnd: HWND) -> Result<()> {
    let enabled = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|s| s.transmission.is_enabled())
    });
    let menu = unsafe { CreatePopupMenu() }?;
    let result = (|| -> Result<()> {
        for (id, label) in [
            (1, "设备音量…"),
            (
                2,
                if enabled {
                    "暂停音频"
                } else {
                    "启用音频"
                },
            ),
            (3, "调高所有手机音量"),
            (4, "调低所有手机音量"),
            (5, "显示主窗口"),
            (6, "服务信息"),
            (7, "设置"),
        ] {
            unsafe { AppendMenuW(menu, MF_STRING, id, &HSTRING::from(label)) }?;
        }
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, w!("")) }?;
        unsafe { AppendMenuW(menu, MF_STRING, 8, w!("退出")) }?;
        let _ = unsafe { SetForegroundWindow(hwnd) };
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }?;
        let id = unsafe {
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
        .0;
        let command = match id {
            1 | 5 => Some(Command::Show(Page::Receivers)),
            2 => Some(Command::Toggle),
            3 => Some(Command::AdjustAll(1)),
            4 => Some(Command::AdjustAll(-1)),
            6 => Some(Command::Show(Page::Information)),
            7 => Some(Command::Show(Page::Settings)),
            8 => Some(Command::Exit),
            _ => None,
        };
        if let Some(command) = command {
            send(command);
        }
        Ok(())
    })();
    let _ = unsafe { DestroyMenu(menu) };
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
    result
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if STATE.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|s| s.taskbar_created != 0 && message == s.taskbar_created)
    }) {
        if let Err(error) = add_icon(hwnd) {
            tracing::warn!(%error, "could not restore tray icon");
        }
        return LRESULT(0);
    }
    if message == CALLBACK {
        match lparam.0 as u32 & 0xffff {
            WM_LBUTTONUP | NIN_SELECT => send(Command::Show(Page::Receivers)),
            WM_CONTEXTMENU | WM_RBUTTONUP => {
                if let Err(error) = menu(hwnd) {
                    tracing::warn!(%error, "could not open tray menu");
                }
            }
            _ => {}
        }
        LRESULT(0)
    } else {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}
