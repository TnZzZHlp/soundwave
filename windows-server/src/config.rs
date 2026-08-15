use std::{
    env,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "audio-stream-server",
    version,
    about = "Stream Windows system audio to Android over LAN QUIC"
)]
pub struct ServerArgs {
    /// UDP address on which the QUIC server listens.
    #[arg(long, default_value = "0.0.0.0:48400")]
    pub bind: SocketAddr,

    /// Directory containing the persistent self-signed server certificate and key.
    #[arg(long, value_name = "DIRECTORY")]
    pub identity_dir: Option<PathBuf>,

    /// IPv4 address advertised in the pairing QR code without changing the listen socket.
    #[arg(long, value_name = "IPv4")]
    pub pairing_host: Option<Ipv4Addr>,

    /// Debug mode: capture WASAPI loopback into this raw PCM file instead of serving a client.
    #[arg(long, value_name = "FILE")]
    pub capture_to: Option<PathBuf>,

    /// Duration for --capture-to, in seconds.
    #[arg(long, default_value_t = 10, requires = "capture_to")]
    pub capture_seconds: u64,
}

pub fn default_identity_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Soundwave")
}
