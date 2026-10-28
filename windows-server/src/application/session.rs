use std::sync::{Arc, atomic::Ordering};

use anyhow::Context;
use quinn::{Connection, VarInt};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    audio::transmission::AudioTransmissionGate,
    network::server::{self, StreamCounters},
    ui::ServerUiReporter,
};

use super::{hub::CaptureHub, registry::ReceiverRegistry};

/// Serves one receiver for as long as its QUIC connection lives.
///
/// Every receiver owns its handshake, packet duration, sequence counter and
/// counters, so a slow or lossy phone only degrades its own stream. The
/// captured audio itself comes from the shared [`CaptureHub`].
pub async fn run(
    connection: Connection,
    hub: CaptureHub,
    registry: ReceiverRegistry,
    server_shutdown: CancellationToken,
    ui: Option<ServerUiReporter>,
    transmission: AudioTransmissionGate,
) -> anyhow::Result<()> {
    let peer = connection.remote_address();
    let session_stop = server_shutdown.child_token();
    let (mut control_writer, mut control_reader, frame_duration_ms) =
        server::perform_handshake(&connection, &session_stop).await?;

    let blocks = hub
        .subscribe()
        .context("could not subscribe the receiver to shared capture")?;
    let counters = Arc::new(StreamCounters::default());
    // One outbound control queue per receiver carries both its own AudioIdle
    // notices and the server-wide volume adjustments.
    let (control_sender, control_receiver) = tokio::sync::mpsc::channel(16);
    let network = server::spawn_datagram_sender(
        connection.clone(),
        blocks,
        frame_duration_ms,
        session_stop.clone(),
        counters.clone(),
        transmission,
        control_sender.clone(),
    );

    let guard = registry.register(peer, control_sender);
    if let Some(ui) = ui.as_ref() {
        ui.streaming_to(&registry.peers());
    }
    println!("\nClient connected:\n{peer}\n\nStreaming...");

    server::process_control_messages(
        &mut control_reader,
        &mut control_writer,
        &connection,
        &session_stop,
        control_receiver,
    )
    .await;

    session_stop.cancel();
    connection.close(VarInt::from_u32(0), b"stream stopped");

    match network.await {
        Ok(Err(error)) => warn!(%error, "QUIC packet task ended with an error"),
        Err(error) => warn!(%error, "QUIC packet task was cancelled"),
        Ok(Ok(())) => {}
    }

    // Deregister before stopping capture so the UI never shows a receiver whose
    // session has already ended.
    drop(guard);
    if let Some(ui) = ui.as_ref() {
        if registry.len() == 0 {
            ui.waiting_for_client();
        } else {
            ui.streaming_to(&registry.peers());
        }
    }
    hub.unsubscribe().await;

    info!(
        peer = %peer,
        packets = counters.packets_sent.load(Ordering::Relaxed),
        network_dropped = counters.packets_dropped.load(Ordering::Relaxed),
        capture_dropped = hub.dropped_blocks(),
        "receiver stream stopped"
    );
    println!(
        "Client disconnected. Packets: {}  Dropped: {}",
        counters.packets_sent.load(Ordering::Relaxed),
        counters.packets_dropped.load(Ordering::Relaxed),
    );
    Ok(())
}
