#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod application;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod config;
#[cfg(any(windows, test))]
mod loudness;
#[cfg(windows)]
mod network;
#[cfg(windows)]
mod pairing;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
fn main() {
    if let Err(error) = application::desktop::run() {
        ui::show_startup_error(&format!("{error:#}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "audio-stream-server only supports Windows because it captures WASAPI loopback audio."
    );
}
