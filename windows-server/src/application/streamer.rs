use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use crossbeam_channel::bounded;
use quinn::{Connection, Endpoint, VarInt};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    audio::{capture::spawn_loopback_capture, device::default_render_device_info},
    config::{ServerArgs, default_identity_dir},
    network::server::{self, FRAME_QUEUE_CAPACITY, StreamCounters},
};

pub async fn run(args: ServerArgs) -> anyhow::Result<()> {
    let device =
        default_render_device_info().context("could not open the default Windows output device")?;
    let identity_dir = args
        .identity_dir
        .clone()
        .unwrap_or_else(default_identity_dir);
    let (server_config, identity) =
        audio_stream_transport::load_or_create_server_config(&identity_dir)?;
    let endpoint = Endpoint::server(server_config, args.bind)
        .with_context(|| format!("could not bind QUIC UDP socket at {}", args.bind))?;

    println!(
        "Audio Stream Server\n\nDevice:\n{}\nMix format:\n{} Hz / {} channels / {} bit {}\nCapture format:\n48000 Hz / Stereo / i16\n\nServer:\n{}\n\nTLS certificate fingerprint (enter this in Android when connecting):\n{}\nIdentity directory:\n{}\n\nWaiting for Android client...",
        device.friendly_name,
        device.mix_sample_rate,
        device.mix_channels,
        device.mix_bits_per_sample,
        device.mix_sample_type,
        args.bind,
        audio_stream_transport::format_fingerprint(&identity.sha256_fingerprint),
        identity_dir.display(),
    );

    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });

    loop {
        let incoming = tokio::select! {
            _ = shutdown.cancelled() => break,
            incoming = endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else {
            break;
        };

        match incoming.await {
            Ok(connection) => {
                if let Err(error) = serve_client(connection, shutdown.clone()).await {
                    warn!(%error, "client session ended with an error");
                }
            }
            Err(error) => warn!(%error, "incoming QUIC connection was rejected"),
        }
    }

    endpoint.close(VarInt::from_u32(0), b"server shutdown");
    endpoint.wait_idle().await;
    println!("Audio Stream Server stopped.");
    Ok(())
}

async fn serve_client(
    connection: Connection,
    server_shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let peer = connection.remote_address();
    let session_stop = server_shutdown.child_token();
    let (mut control_writer, mut control_reader, frame_duration_ms) =
        server::perform_handshake(&connection, &session_stop).await?;

    let (frame_sender, frame_receiver) = bounded(FRAME_QUEUE_CAPACITY);
    let counters = Arc::new(StreamCounters::default());
    let capture_dropped = Arc::new(AtomicU64::new(0));
    let capture = spawn_loopback_capture(
        frame_sender,
        frame_receiver.clone(),
        session_stop.clone(),
        capture_dropped.clone(),
    )
    .context("could not start WASAPI loopback thread")?;
    let network = server::spawn_datagram_sender(
        connection.clone(),
        frame_receiver,
        frame_duration_ms,
        session_stop.clone(),
        counters.clone(),
    )
    .context("could not start QUIC packet thread")?;

    println!("\nClient connected:\n{peer}\n\nStreaming...");
    server::process_control_messages(
        &mut control_reader,
        &mut control_writer,
        &connection,
        &session_stop,
    )
    .await;

    session_stop.cancel();
    connection.close(VarInt::from_u32(0), b"stream stopped");

    let worker_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        capture
            .join()
            .map_err(|_| anyhow::anyhow!("WASAPI capture thread panicked"))??;
        network
            .join()
            .map_err(|_| anyhow::anyhow!("QUIC packet thread panicked"))??;
        Ok(())
    })
    .await
    .context("audio worker join task panicked")?;
    if let Err(error) = worker_result {
        warn!(%error, "audio worker ended with an error");
    }

    info!(
        peer = %peer,
        packets = counters.packets_sent.load(Ordering::Relaxed),
        network_dropped = counters.packets_dropped.load(Ordering::Relaxed),
        capture_dropped = capture_dropped.load(Ordering::Relaxed),
        "client stream stopped"
    );
    println!(
        "Client disconnected. Packets: {}  Dropped: {}",
        counters.packets_sent.load(Ordering::Relaxed),
        counters.packets_dropped.load(Ordering::Relaxed) + capture_dropped.load(Ordering::Relaxed),
    );
    Ok(())
}
