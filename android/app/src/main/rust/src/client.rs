use std::{net::SocketAddr, sync::Arc, time::Duration};

use audio_stream_protocol::{ControlMessage, PROTOCOL_VERSION, StreamInfo};
use audio_stream_transport::{
    ControlReader, ControlWriter, DatagramReceiver, make_pinned_client_config,
};
use quinn::{Connection, Endpoint, VarInt};

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

/// Runs one QUIC session and retries forever with an exponential backoff so a
/// restarting server or recovering network is picked up without manual reconnect.
pub async fn run_session(
    address: SocketAddr,
    fingerprint: [u8; 32],
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
) {
    let mut retry_attempt = 0_u32;
    loop {
        let result = run_session_inner(address, fingerprint, ring.clone(), state.clone()).await;
        ring.clear();
        // A session that ended cleanly means the server was reachable moments
        // ago; restart the backoff so a briefly offline server is caught fast.
        match result {
            Ok(()) => {
                retry_attempt = 0;
                // A remote close is still a reconnecting state. Publish it before
                // the backoff so Android can release live-playback resources.
                state.set(ClientState::Connecting);
            }
            Err(error) => state.set_error(&error),
        }
        tokio::time::sleep(retry_delay(retry_attempt)).await;
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
) -> Result<(), ClientError> {
    let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
    endpoint.set_default_client_config(make_pinned_client_config(fingerprint)?);
    let connecting = endpoint
        .connect(address, "soundwave.local")
        .map_err(|error| {
            ClientError::Runtime(format!("could not start QUIC connection: {error}"))
        })?;
    let connection = connecting
        .await
        .map_err(|error| ClientError::Runtime(error.to_string()))?;

    let result = connected_loop(&connection, ring, state).await;
    connection.close(VarInt::from_u32(0), b"android disconnected");
    endpoint.close(VarInt::from_u32(0), b"android disconnected");
    result
}

async fn connected_loop(
    connection: &Connection,
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
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

    let mut pipeline = ReceiverPipeline::new(stream, ring.clone(), state.clone())?;
    let mut newest_sequence = None;
    let mut idle_boundary = None;
    let mut idle_deadline = None;
    let datagrams = DatagramReceiver::new(connection.clone())?;
    state.set(ClientState::Connected);

    loop {
        tokio::select! {
            _ = connection.closed() => return Ok(()),
            _ = tokio::time::sleep_until(idle_deadline.unwrap_or_else(tokio::time::Instant::now)), if idle_deadline.is_some() => {
                idle_deadline = None;
                state.set_audio_active(false);
            }
            packet = datagrams.receive() => {
                let packet = packet?;
                if let Some(boundary) = idle_boundary {
                    if !sequence_before(packet.sequence, boundary) {
                        pipeline.resume(boundary);
                        idle_boundary = None;
                        idle_deadline = None;
                    }
                }
                let sequence = packet.sequence;
                state.set_rtt_ms(
                    u64::try_from(connection.rtt().as_millis()).unwrap_or(u64::MAX),
                );
                if pipeline.handle_packet(packet) {
                    if newest_sequence.is_none_or(|newest| !sequence_before(sequence, newest)) {
                        newest_sequence = Some(sequence);
                    }
                    if idle_boundary.is_none() {
                        state.set_audio_active(true);
                    }
                }
            }
            message = reader.receive() => {
                match message? {
                    ControlMessage::Stop => return Ok(()),
                    ControlMessage::AudioIdle { next_sequence } => {
                        if newest_sequence.is_none_or(|newest| sequence_before(newest, next_sequence)) {
                            idle_boundary = Some(next_sequence);
                            idle_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(150));
                        }
                    }
                    ControlMessage::AdjustVolume { steps } => {
                        state.add_volume_adjustment(steps);
                    }
                    unexpected => return Err(ClientError::Runtime(format!("unexpected control message: {unexpected:?}"))),
                }
            }
        }
    }
}

fn sequence_before(sequence: u32, reference: u32) -> bool {
    let distance = sequence.wrapping_sub(reference);
    distance != 0 && distance >= (1_u32 << 31)
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
    use std::{sync::Arc, time::Instant};

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
    async fn idle_connection_resumes_reordered_audio_without_losing_first_packet() {
        let (config, identity) = make_server_config().unwrap();
        let server = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let ring = Arc::new(PcmRingBuffer::new(PCM_RING_CAPACITY_SAMPLES));
        let state = Arc::new(SharedState::default());
        let client = tokio::spawn(run_session(
            server.local_addr().unwrap(),
            identity.sha256_fingerprint,
            ring.clone(),
            state.clone(),
        ));
        let connection = server.accept().await.unwrap().await.unwrap();
        let (send, recv) = connection.accept_bi().await.unwrap();
        let mut writer = ControlWriter::new(send);
        let mut reader = ControlReader::new(recv);
        assert_eq!(
            reader.receive().await.unwrap(),
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            }
        );
        for message in [
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            ControlMessage::StreamInfo(StreamInfo {
                sample_rate: 48_000,
                channels: 2,
                sample_format: SampleFormat::I16Le,
                frame_duration_ms: 5,
            }),
            ControlMessage::Start,
        ] {
            writer.send(&message).await.unwrap();
        }
        let sender = DatagramSender::new(connection.clone()).unwrap();
        for sequence in 0..6 {
            sender
                .send(&audio_stream_protocol::AudioPacket::new(
                    sequence,
                    u64::from(sequence) * 240,
                    vec![0; 960],
                ))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.received_packets() < 6 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(state.audio_active());
        writer
            .send(&ControlMessage::AudioIdle { next_sequence: 6 })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.audio_active() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.state(), ClientState::Connected);

        for sequence in [5, 7, 6, 8, 9] {
            let payload: Vec<u8> = std::iter::repeat_n((sequence as i16 + 1).to_le_bytes(), 480)
                .flatten()
                .collect();
            sender
                .send(&audio_stream_protocol::AudioPacket::new(
                    sequence,
                    u64::from(sequence) * 240,
                    payload,
                ))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.audio_active() || ring.available_samples() != 1_920 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut first_packet = [0; 480];
        assert_eq!(ring.read_into(&mut first_packet), 480);
        assert_eq!(first_packet, [7; 480]);

        writer
            .send(&ControlMessage::AudioIdle { next_sequence: 6 })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(state.audio_active());
        assert_eq!(state.lost_packets(), 0);
        client.abort();
        assert!(client.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn native_client_receives_audio_and_retries_after_remote_close() {
        let (server_config, identity) = make_server_config().unwrap();
        let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_address = server.local_addr().unwrap();
        let (close_server, wait_for_close) = tokio::sync::oneshot::channel();
        let (second_connection_seen, second_connection_observed) = tokio::sync::oneshot::channel();

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
            writer
                .send(&ControlMessage::AdjustVolume { steps: 1 })
                .await
                .unwrap();

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
                tokio::task::yield_now().await;
            }
            wait_for_close.await.unwrap();
            let close_at = Instant::now();
            connection.close(VarInt::from_u32(0), b"test server stopped");
            connection.closed().await;

            let incoming = tokio::time::timeout(Duration::from_secs(3), server.accept())
                .await
                .expect("server should accept a retrying client")
                .expect("server endpoint should remain open");
            let second_connection = incoming
                .await
                .expect("server should establish the retrying connection");
            second_connection_seen
                .send(close_at.elapsed())
                .expect("test should still await the retrying connection");
            second_connection.close(VarInt::from_u32(0), b"test server stopped");
            second_connection.closed().await;
        });

        let ring = Arc::new(PcmRingBuffer::new(PCM_RING_CAPACITY_SAMPLES));
        let state = Arc::new(SharedState::default());
        state.begin_connecting();
        let client_task = tokio::spawn(run_session(
            server_address,
            identity.sha256_fingerprint,
            ring.clone(),
            state.clone(),
        ));

        let volume_adjustment = tokio::time::timeout(Duration::from_secs(2), async {
            let mut volume_adjustment = 0;
            loop {
                volume_adjustment += state.take_volume_adjustment();
                if state.received_packets() >= 12
                    && ring.available_samples() >= PCM_RING_CAPACITY_SAMPLES
                    && volume_adjustment != 0
                {
                    break volume_adjustment;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("client should receive test datagrams and volume adjustment");
        assert_eq!(state.state(), ClientState::Connected);
        assert_eq!(state.jitter_buffer_ms(), 20);
        assert_eq!(volume_adjustment, 1);
        assert!(ring.available_samples() >= PCM_RING_CAPACITY_SAMPLES);

        close_server.send(()).unwrap();
        let state_after_close = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = state.state();
                if state != ClientState::Connected {
                    break state;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("client should leave connected state after server closes");
        assert!(matches!(
            state_after_close,
            ClientState::Connecting | ClientState::Error
        ));
        let second_connection_elapsed = tokio::time::timeout(
            RETRY_INITIAL_DELAY + Duration::from_secs(2),
            second_connection_observed,
        )
        .await
        .expect("client should retry after the documented backoff")
        .expect("server should observe the retrying connection");
        assert!(
            second_connection_elapsed >= RETRY_INITIAL_DELAY,
            "retrying connection started after {second_connection_elapsed:?}, before the documented backoff"
        );

        client_task.abort();
        assert!(client_task.await.unwrap_err().is_cancelled());
        server_task.await.unwrap();
    }
}
