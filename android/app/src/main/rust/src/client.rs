use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use audio_stream_protocol::{ControlMessage, PROTOCOL_VERSION, StreamInfo};
use audio_stream_transport::{
    ControlReader, ControlWriter, DatagramReceiver, make_pinned_client_config,
};
use quinn::{Connection, Endpoint, VarInt};
use tokio_util::sync::CancellationToken;

use crate::{
    error::ClientError,
    receiver::ReceiverPipeline,
    ring_buffer::PcmRingBuffer,
    state::{ClientState, SharedState},
};

/// Delay before the first reconnection attempt after a session ends.
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Upper bound for the exponential backoff between reconnection attempts.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

/// Runs one QUIC session and, unless [`stop`](CancellationToken) was
/// cancelled (which only a user-initiated disconnect does), retries forever
/// with an exponential backoff so a restarting server or recovering network
/// is picked up without a manual reconnect.
pub async fn run_session(
    address: SocketAddr,
    fingerprint: [u8; 32],
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
    stop: CancellationToken,
) {
    let mut retry_attempt = 0_u32;
    loop {
        let result = run_session_inner(
            address,
            fingerprint,
            ring.clone(),
            state.clone(),
            stop.clone(),
        )
        .await;
        ring.clear();
        if stop.is_cancelled() {
            state.set(ClientState::Disconnected);
            return;
        }
        // A session that ended cleanly means the server was reachable moments
        // ago; restart the backoff so a briefly offline server is caught fast.
        match result {
            Ok(()) => retry_attempt = 0,
            Err(error) => state.set_error(&error),
        }
        // Cancellation during the wait exits immediately instead of after the
        // full backoff delay.
        tokio::select! {
            () = stop.cancelled() => {
                state.set(ClientState::Disconnected);
                return;
            }
            () = tokio::time::sleep(retry_delay(retry_attempt)) => {}
        }
        retry_attempt = retry_attempt.saturating_add(1);
        state.set(ClientState::Connecting);
    }
}

/// Exponential backoff starting at 1 s, doubling per failed attempt, capped
/// at 10 s.
fn retry_delay(attempt: u32) -> Duration {
    RETRY_INITIAL_DELAY
        .checked_mul(1_u32 << attempt.min(4))
        .unwrap_or(RETRY_MAX_DELAY)
        .min(RETRY_MAX_DELAY)
}

async fn run_session_inner(
    address: SocketAddr,
    fingerprint: [u8; 32],
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
    stop: CancellationToken,
) -> Result<(), ClientError> {
    let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
    endpoint.set_default_client_config(make_pinned_client_config(fingerprint)?);
    let connecting = endpoint
        .connect(address, "soundwave.local")
        .map_err(|error| {
            ClientError::Runtime(format!("could not start QUIC connection: {error}"))
        })?;
    let connection = tokio::select! {
        connection = connecting => connection.map_err(|error| ClientError::Runtime(error.to_string()))?,
        () = stop.cancelled() => return Ok(()),
    };

    let result = connected_loop(&connection, ring, state, stop.clone()).await;
    connection.close(VarInt::from_u32(0), b"android disconnected");
    endpoint.close(VarInt::from_u32(0), b"android disconnected");
    result
}

async fn connected_loop(
    connection: &Connection,
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
    stop: CancellationToken,
) -> Result<(), ClientError> {
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| ClientError::Runtime(error.to_string()))?;
    let mut writer = ControlWriter::new(send);
    let mut reader = ControlReader::new(recv);
    writer
        .send(&ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;

    expect_hello(&mut reader).await?;
    let stream = expect_stream_info(&mut reader).await?;
    expect_start(&mut reader).await?;

    let mut pipeline = ReceiverPipeline::new(stream, ring, state.clone())?;
    let datagrams = DatagramReceiver::new(connection.clone())?;
    state.set(ClientState::Connected);

    let started = Instant::now();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = connection.closed() => return Ok(()),
            packet = datagrams.receive() => pipeline.handle_packet(packet?),
            message = reader.receive() => {
                match message? {
                    ControlMessage::Stop => return Ok(()),
                    ControlMessage::Ping { timestamp } => writer.send(&ControlMessage::Pong { timestamp }).await?,
                    ControlMessage::Pong { timestamp } => {
                        let now_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        if now_ms >= timestamp {
                            state.set_rtt_ms(now_ms - timestamp);
                        }
                    }
                    unexpected => return Err(ClientError::Runtime(format!("unexpected control message: {unexpected:?}"))),
                }
            }
            _ = heartbeat.tick() => {
                let timestamp =
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                writer.send(&ControlMessage::Ping { timestamp }).await?;
            }
        }
    }
}

async fn expect_hello(reader: &mut ControlReader) -> Result<(), ClientError> {
    match reader.receive().await? {
        ControlMessage::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {
            Ok(())
        }
        ControlMessage::Hello { protocol_version } => Err(ClientError::Runtime(format!(
            "server uses unsupported protocol version {protocol_version}"
        ))),
        message => Err(ClientError::Runtime(format!(
            "expected server Hello, got {message:?}"
        ))),
    }
}

async fn expect_stream_info(reader: &mut ControlReader) -> Result<StreamInfo, ClientError> {
    match reader.receive().await? {
        ControlMessage::StreamInfo(stream) => Ok(stream),
        message => Err(ClientError::Runtime(format!(
            "expected StreamInfo, got {message:?}"
        ))),
    }
}

async fn expect_start(reader: &mut ControlReader) -> Result<(), ClientError> {
    match reader.receive().await? {
        ControlMessage::Start => Ok(()),
        message => Err(ClientError::Runtime(format!(
            "expected Start, got {message:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use audio_stream_protocol::{SampleFormat, StreamInfo};
    use audio_stream_transport::{DatagramSender, make_server_config};
    use quinn::Endpoint;

    use super::*;
    use crate::ring_buffer::PCM_RING_CAPACITY_SAMPLES;

    #[test]
    fn retry_delay_backs_off_and_caps_at_max() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(3), Duration::from_secs(8));
        assert_eq!(retry_delay(4), Duration::from_secs(10));
        assert_eq!(retry_delay(5), Duration::from_secs(10));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn native_client_receives_pinned_quic_audio() {
        let (server_config, identity) = make_server_config().unwrap();
        let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_address = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            let (send, recv) = connection.accept_bi().await.unwrap();
            let mut reader = ControlReader::new(recv);
            let mut writer = ControlWriter::new(send);
            assert_eq!(
                reader.receive().await.unwrap(),
                ControlMessage::Hello {
                    protocol_version: PROTOCOL_VERSION
                }
            );
            writer
                .send(&ControlMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .await
                .unwrap();
            writer
                .send(&ControlMessage::StreamInfo(StreamInfo {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_format: SampleFormat::I16Le,
                    frame_duration_ms: 5,
                }))
                .await
                .unwrap();
            writer.send(&ControlMessage::Start).await.unwrap();

            let sender = DatagramSender::new(connection.clone()).unwrap();
            let payload = vec![0_u8; 960];
            for sequence in 0..20 {
                sender
                    .send(&audio_stream_protocol::AudioPacket::new(
                        sequence,
                        u64::from(sequence) * 240,
                        payload.clone(),
                    ))
                    .unwrap();
            }
            connection.closed().await;
        });

        let ring = Arc::new(PcmRingBuffer::new(PCM_RING_CAPACITY_SAMPLES));
        let state = Arc::new(SharedState::default());
        let stop = CancellationToken::new();
        state.begin_connecting();
        let client_task = tokio::spawn(run_session(
            server_address,
            identity.sha256_fingerprint,
            ring.clone(),
            state.clone(),
            stop.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while state.received_packets() < 12 || ring.available_samples() < 9_600 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("client should receive test datagrams");
        assert_eq!(state.state(), ClientState::Connected);
        assert!(ring.available_samples() >= 9_600);

        stop.cancel();
        client_task.await.unwrap();
        server_task.await.unwrap();
    }
}
