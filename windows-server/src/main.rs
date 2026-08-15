#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod application;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod config;
#[cfg(windows)]
mod network;
#[cfg(windows)]
mod pairing;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use clap::Parser;
#[cfg(windows)]
use config::ServerArgs;

#[cfg(windows)]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    if let Err(error) = run_application().await {
        ui::show_startup_error(&error.to_string());
        std::process::exit(1);
    }
}

#[cfg(windows)]
async fn run_application() -> anyhow::Result<()> {
    let args = ServerArgs::parse();
    if let Some(path) = args.capture_to.as_deref() {
        if args.capture_seconds == 0 {
            anyhow::bail!("--capture-seconds must be greater than zero");
        }
        println!("Capturing WASAPI loopback to {}...", path.display());
        let path = path.to_owned();
        let capture_path = path.clone();
        let duration = Duration::from_secs(args.capture_seconds);
        tokio::task::spawn_blocking(move || audio::capture::record_to_pcm(&capture_path, duration))
            .await
            .context("debug capture task panicked")??;
        println!(
            "Wrote 48 kHz stereo i16 little-endian PCM to {}",
            path.display()
        );
        ui::show_capture_complete(&path);
        return Ok(());
    }

    application::streamer::run(args).await
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "audio-stream-server only supports Windows because it captures WASAPI loopback audio."
    );
}
