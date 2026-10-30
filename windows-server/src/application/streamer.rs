use std::sync::mpsc::SyncSender;

use anyhow::Context;
use quinn::{Endpoint, VarInt};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    audio::{device::default_render_device_info, transmission::AudioTransmissionGate},
    config::{ServerArgs, default_identity_dir},
    pairing,
};

use super::{
    hub::CaptureHub,
    registry::ReceiverRegistry,
    session,
    state::{ServerInfo, ServerState},
};

pub async fn run(
    args: ServerArgs,
    shutdown: CancellationToken,
    ready: SyncSender<Result<ServerState, String>>,
) -> anyhow::Result<()> {
    let device = default_render_device_info().context("无法打开 Windows 默认输出设备")?;
    let identity_dir = args
        .identity_dir
        .clone()
        .unwrap_or_else(default_identity_dir);
    let (server_config, identity) =
        audio_stream_transport::load_or_create_server_config(&identity_dir)
            .context("无法加载或创建服务端证书和私钥")?;
    let endpoint = Endpoint::server(server_config, args.bind)
        .with_context(|| format!("无法在 {} 绑定 QUIC UDP 套接字", args.bind))?;
    let endpoint_address = endpoint.local_addr().context("无法确定 QUIC 监听地址")?;
    let fingerprint = audio_stream_transport::format_fingerprint(&identity.sha256_fingerprint);
    let pairing = pairing::resolve_pairing_info(
        args.bind,
        endpoint_address,
        args.pairing_host,
        identity.sha256_fingerprint,
    )
    .map_err(|error| error.to_string());

    println!(
        "Audio Stream Server\n\nDevice:\n{}\nMix format:\n{} Hz / {} channels / {} bit {}\nCapture format:\n48000 Hz / Stereo / i16\n\nServer:\n{}\n\nTLS certificate fingerprint (enter this in Android when connecting):\n{}\nIdentity directory:\n{}\n\nWaiting for Android client...",
        device.friendly_name,
        device.mix_sample_rate,
        device.mix_channels,
        device.mix_bits_per_sample,
        device.mix_sample_type,
        endpoint_address,
        fingerprint,
        identity_dir.display(),
    );

    let transmission = AudioTransmissionGate::enabled();
    let hub = CaptureHub::new(transmission.clone());
    let registry = ReceiverRegistry::default();
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });

    ready
        .send(Ok(ServerState::new(
            ServerInfo {
                device_name: device.friendly_name.clone(),
                mix_sample_rate: device.mix_sample_rate,
                mix_channels: device.mix_channels,
                mix_bits_per_sample: device.mix_bits_per_sample,
                mix_sample_type: device.mix_sample_type.clone(),
                bind: endpoint_address,
                fingerprint,
                identity_dir: identity_dir.clone(),
                pairing,
            },
            shutdown.clone(),
            transmission.clone(),
            registry.clone(),
        )))
        .map_err(|_| anyhow::anyhow!("桌面界面在服务初始化前已退出"))?;
    drop(ready);

    let mut sessions = tokio::task::JoinSet::new();
    loop {
        let incoming = tokio::select! {
            _ = shutdown.cancelled() => break,
            incoming = endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else {
            break;
        };

        let hub = hub.clone();
        let registry = registry.clone();
        let server_shutdown = shutdown.clone();
        let transmission = transmission.clone();
        // The handshake runs in the session task so a slow or stalled peer
        // cannot delay accepting the next receiver.
        sessions.spawn(async move {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    warn!(%error, "incoming QUIC connection was rejected");
                    return;
                }
            };
            if let Err(error) =
                session::run(connection, hub, registry, server_shutdown, transmission).await
            {
                warn!(%error, "receiver session ended with an error");
            }
        });

        // Reap finished sessions so their errors are observed and the set does
        // not grow without bound over a long server lifetime.
        while sessions.try_join_next().is_some() {}
    }

    // Stopping the server closes every receiver connection; wait for the
    // sessions to unwind so capture is joined before the endpoint goes idle.
    sessions.shutdown().await;

    endpoint.close(VarInt::from_u32(0), b"server shutdown");
    endpoint.wait_idle().await;
    println!("Audio Stream Server stopped.");
    Ok(())
}
