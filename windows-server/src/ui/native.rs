use std::path::Path;

use anyhow::Result;
use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            IDOK, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_OKCANCEL,
            MessageBoxW, SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
    },
    core::{HSTRING, w},
};

pub(super) fn hwnd(window: &Window) -> Result<HWND> {
    match HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("无法获取 GPUI 窗口句柄：{error}"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        _ => anyhow::bail!("GPUI 未创建 Windows 窗口"),
    }
}

pub(super) fn hide(window: &Window) {
    if let Ok(hwnd) = hwnd(window) {
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

pub(super) fn show(window: &Window) {
    if let Ok(hwnd) = hwnd(window) {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        let _ = unsafe { SetForegroundWindow(hwnd) };
    }
}

pub(super) fn confirm_calibration(window: &Window) -> bool {
    unsafe {
        MessageBoxW(
            hwnd(window).ok(),
            w!(
                "是否暂停音频传输，并以所选手机为基准，匹配所有手机扬声器的响度？\n\n请先试听，将基准手机调至舒适音量。其他手机将从低音量开始，最高不超过校准上限。请将麦克风放在聆听位置，并关闭自动增益和音效增强。请断开耳机和外接音频输出设备。\n\n系统媒体音量的变化也会影响其他应用。"
            ),
            w!("校准相对响度"),
            MB_OKCANCEL | MB_ICONWARNING,
        ) == IDOK
    }
}

pub(crate) fn show_startup_error(error: &str) {
    let text = HSTRING::from(format!("Soundwave 服务启动失败。\n\n{error}"));
    let _ = unsafe { MessageBoxW(None, &text, w!("Soundwave 服务端"), MB_OK | MB_ICONERROR) };
}

pub(crate) fn show_capture_complete(path: &Path) {
    let text = HSTRING::from(format!(
        "已保存 48 kHz 双声道 i16 小端 PCM 音频：\n{}",
        path.display()
    ));
    let _ = unsafe {
        MessageBoxW(
            None,
            &text,
            w!("Soundwave 服务端"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}
