use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::Context;
use audio_stream_protocol::{
    AudioPacket, CHANNELS, ControlMessage, PROTOCOL_VERSION, SAMPLE_RATE, SampleFormat, StreamInfo,
};
use audio_stream_transport::{ControlReader, ControlWriter, DatagramSender, TransportError};
use bytes::Bytes;
use quinn::Connection;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::audio::PcmBlock;

use super::silence::SilenceDetector;

/// Smallest shared-capture buffer that still keeps one 10 ms block available
/// for every receiver without accumulating delay.
pub const FRAME_QUEUE_CAPACITY: usize = 1;

pub use crate::audio::transmission::AudioTransmissionGate;

#[derive(Default)]
pub struct StreamCounters {
    pub packets_sent: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub blocks_skipped: AtomicU64,
}

pub async fn perform_handshake(
    connection: &Connection,
    shutdown: &CancellationToken,
) -> anyhow::Result<(ControlWriter, ControlReader, u8)> {
    let (send, recv) = tokio::select! {
        streams = connection.accept_bi() => streams.context("client did not open a control stream")?,
        _ = shutdown.cancelled() => anyhow::bail!("server is shutting down"),
        _ = connection.closed() => anyhow::bail!("client disconnected before opening control stream"),
    };
    let mut writer = ControlWriter::new(send);
    let mut reader = ControlReader::new(recv);
    let hello = tokio::select! {
        message = reader.receive() => message.context("failed to read client hello")?,
        _ = shutdown.cancelled() => anyhow::bail!("server is shutting down"),
        _ = connection.closed() => anyhow::bail!("client disconnected during control handshake"),
    };

    match hello {
        ControlMessage::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {}
        ControlMessage::Hello { protocol_version } => {
            anyhow::bail!("unsupported client protocol version {protocol_version}");
        }
        other => anyhow::bail!("expected Hello from client, got {other:?}"),
    }

    let datagrams = DatagramSender::new(connection.clone())
        .context("client does not support QUIC datagrams")?;
    let frame_duration_ms = choose_packet_duration_ms(datagrams.max_payload_len())?;
    writer
        .send(&ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .await
        .context("failed to send server hello")?;
    writer
        .send(&ControlMessage::StreamInfo(StreamInfo {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            sample_format: SampleFormat::I16Le,
            frame_duration_ms,
        }))
        .await
        .context("failed to send stream information")?;
    writer
        .send(&ControlMessage::Start)
        .await
        .context("failed to send start message")?;

    Ok((writer, reader, frame_duration_ms))
}

pub struct ReceiverOutput {
    pub controls: tokio::sync::mpsc::Sender<ControlMessage>,
    pub playback: Arc<AtomicBool>,
}

/// Sends shared capture blocks to one receiver.
///
/// Every receiver runs its own sender task with its own packetizer, sequence
/// counter and counters, so a congested phone only loses its own audio. Capture
/// blocks arrive as shared `Arc`s; a receiver that falls behind gets `Lagged`
/// and drops the blocks it missed instead of slowing the other receivers.
pub fn spawn_datagram_sender(
    connection: Connection,
    mut frames: broadcast::Receiver<Arc<PcmBlock>>,
    frame_duration_ms: u8,
    stop: CancellationToken,
    counters: Arc<StreamCounters>,
    transmission: AudioTransmissionGate,
    output: ReceiverOutput,
) -> tokio::task::JoinHandle<Result<(), TransportError>> {
    let ReceiverOutput {
        controls: outbound,
        playback,
    } = output;
    tokio::runtime::Handle::current().spawn(async move {
        let sender = match DatagramSender::new(connection) {
            Ok(sender) => sender,
            Err(error) => {
                stop.cancel();
                return Err(error);
            }
        };
        let mut packetizer = Packetizer::new(frame_duration_ms);
        let mut silence = SilenceDetector::new();
        let poll = std::time::Duration::from_millis(100);

        let result = loop {
            if stop.is_cancelled() {
                break Ok(());
            }
            if !transmission.is_enabled() {
                let (_, became_idle) = silence.update(0, true, false);
                packetizer.pending_tail = None;
                if became_idle && !report_idle(&outbound, &packetizer).await {
                    break Ok(());
                }
                // The blocking gate wait must not occupy an async worker.
                let wait = stop.clone();
                let gate = transmission.clone();
                let _ = tokio::task::spawn_blocking(move || gate.wait_until_enabled(&wait)).await;
                continue;
            }

            match tokio::time::timeout(poll, frames.recv()).await {
                Ok(Ok(block)) => {
                    let allowed = playback.load(Ordering::Acquire);
                    let (send, became_idle) = silence.update(
                        block.timestamp,
                        block.samples.iter().all(|&sample| sample == 0),
                        transmission.is_enabled() && allowed,
                    );
                    if became_idle {
                        packetizer.pending_tail = None;
                        if !report_idle(&outbound, &packetizer).await {
                            break Ok(());
                        }
                    }
                    if !send {
                        continue;
                    }
                    let block = Arc::try_unwrap(block).unwrap_or_else(|shared| PcmBlock {
                        timestamp: shared.timestamp,
                        samples: shared.samples,
                    });
                    match packetizer.send_block(&sender, block, &counters, &transmission) {
                        Ok(()) => {}
                        Err(error) => break Err(error),
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    counters
                        .blocks_skipped
                        .fetch_add(skipped, Ordering::Relaxed);
                    packetizer.pending_tail = None;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break Ok(()),
                Err(_elapsed) => continue,
            }
        };

        if result.is_err() {
            stop.cancel();
        }
        result
    })
}

async fn report_idle(
    outbound: &tokio::sync::mpsc::Sender<ControlMessage>,
    packetizer: &Packetizer,
) -> bool {
    outbound
        .send(ControlMessage::AudioIdle {
            next_sequence: packetizer.sequence,
        })
        .await
        .is_ok()
}

/// Chooses the largest whole-millisecond packet duration that QUIC has actually
/// advertised for this connection. This avoids guessing an Ethernet MTU and
/// avoids IP fragmentation. A normal LAN path uses 10 ms; constrained paths use
/// 5 ms or smaller packet splits with the same PCM stream.
pub fn choose_packet_duration_ms(max_payload_bytes: usize) -> anyhow::Result<u8> {
    for duration_ms in [10_u8, 5, 4, 2, 1] {
        let bytes = samples_per_packet(duration_ms)
            .checked_mul(std::mem::size_of::<i16>())
            .context("audio packet size overflow")?;
        if bytes <= max_payload_bytes {
            return Ok(duration_ms);
        }
    }
    anyhow::bail!(
        "QUIC only permits {max_payload_bytes} bytes of audio payload; at least {} bytes are required for 1 ms stereo PCM",
        samples_per_packet(1) * std::mem::size_of::<i16>()
    )
}

const FOUR_MS_PACKET_SAMPLES: usize = samples_per_packet(4);
const FOUR_MS_TAIL_SAMPLES: usize = PcmBlock::SAMPLE_COUNT % FOUR_MS_PACKET_SAMPLES;

struct Packetizer {
    sequence: u32,
    frame_duration_ms: u8,
    samples_per_packet: usize,
    pending_tail: Option<PendingTail>,
}

struct PendingTail {
    timestamp: u64,
    samples: [i16; FOUR_MS_TAIL_SAMPLES],
}

impl Packetizer {
    fn new(frame_duration_ms: u8) -> Self {
        Self {
            sequence: 0,
            frame_duration_ms,
            samples_per_packet: samples_per_packet(frame_duration_ms),
            pending_tail: None,
        }
    }

    fn send_block(
        &mut self,
        sender: &DatagramSender,
        block: PcmBlock,
        counters: &StreamCounters,
        transmission: &AudioTransmissionGate,
    ) -> Result<(), TransportError> {
        if !transmission.is_enabled() {
            return Ok(());
        }

        let mut sequence = self.sequence;
        let mut send_error = None;
        self.for_each_packet(block, |timestamp, samples| {
            if !transmission.is_enabled() {
                return false;
            }
            let mut payload = Vec::with_capacity(std::mem::size_of_val(samples));
            for sample in samples {
                payload.extend_from_slice(&sample.to_le_bytes());
            }
            let packet = AudioPacket::new(sequence, timestamp, Bytes::from(payload));
            sequence = sequence.wrapping_add(1);

            match sender.send(&packet) {
                Ok(()) => {
                    counters.packets_sent.fetch_add(1, Ordering::Relaxed);
                    true
                }
                // A full local QUIC datagram queue is treated as loss. It is
                // preferable to lose this old frame than to buffer it and grow
                // latency.
                Err(TransportError::Quinn(_)) => {
                    counters.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(error) => {
                    send_error = Some(error);
                    false
                }
            }
        });

        self.sequence = sequence;
        match send_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn for_each_packet<F>(&mut self, block: PcmBlock, mut callback: F)
    where
        F: FnMut(u64, &[i16]) -> bool,
    {
        if self.frame_duration_ms == 4 {
            self.for_each_four_ms_packet(block, callback);
            return;
        }

        for (index, samples) in block.samples.chunks(self.samples_per_packet).enumerate() {
            let timestamp =
                block.timestamp + (index * self.samples_per_packet / CHANNELS as usize) as u64;
            if !callback(timestamp, samples) {
                break;
            }
        }
    }

    fn for_each_four_ms_packet<F>(&mut self, block: PcmBlock, mut callback: F)
    where
        F: FnMut(u64, &[i16]) -> bool,
    {
        let mut offset = 0;
        if let Some(tail) = self.pending_tail.take() {
            let expected_timestamp = tail
                .timestamp
                .wrapping_add((FOUR_MS_TAIL_SAMPLES / CHANNELS as usize) as u64);
            if block.timestamp == expected_timestamp {
                let mut samples = [0_i16; FOUR_MS_PACKET_SAMPLES];
                samples[..FOUR_MS_TAIL_SAMPLES].copy_from_slice(&tail.samples);
                samples[FOUR_MS_TAIL_SAMPLES..].copy_from_slice(
                    &block.samples[..FOUR_MS_PACKET_SAMPLES - FOUR_MS_TAIL_SAMPLES],
                );
                if !callback(tail.timestamp, &samples) {
                    return;
                }
                offset = FOUR_MS_PACKET_SAMPLES - FOUR_MS_TAIL_SAMPLES;
            }
        }

        while offset + FOUR_MS_PACKET_SAMPLES <= block.samples.len() {
            let timestamp = block.timestamp + (offset / CHANNELS as usize) as u64;
            if !callback(
                timestamp,
                &block.samples[offset..offset + FOUR_MS_PACKET_SAMPLES],
            ) {
                return;
            }
            offset += FOUR_MS_PACKET_SAMPLES;
        }

        let remaining = block.samples.len() - offset;
        if remaining == 0 {
            return;
        }
        debug_assert_eq!(remaining, FOUR_MS_TAIL_SAMPLES);
        let mut samples = [0_i16; FOUR_MS_TAIL_SAMPLES];
        samples.copy_from_slice(&block.samples[offset..]);
        self.pending_tail = Some(PendingTail {
            timestamp: block.timestamp + (offset / CHANNELS as usize) as u64,
            samples,
        });
    }
}

const fn samples_per_packet(frame_duration_ms: u8) -> usize {
    (SAMPLE_RATE as usize * frame_duration_ms as usize / 1_000) * CHANNELS as usize
}

pub async fn process_control_messages(
    reader: &mut ControlReader,
    writer: &mut ControlWriter,
    connection: &Connection,
    stop: &CancellationToken,
    mut outbound: tokio::sync::mpsc::Receiver<ControlMessage>,
    registry: &crate::application::registry::ReceiverRegistry,
    receiver_id: u64,
) {
    let mut query_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = connection.closed() => break,
            _ = query_tick.tick() => {
                if let Err(error) = registry.request(receiver_id, None) {
                    warn!(%error, "failed to queue device status query");
                }
            }
            Some(message) = outbound.recv() => {
                if let Err(error) = writer.send(&message).await {
                    warn!(%error, "failed to send a server control message");
                    break;
                }
            }
            message = reader.receive() => match message {
                Ok(ControlMessage::Stop) => {
                    debug!("client requested stream stop");
                    break;
                }
                Ok(ControlMessage::DeviceStatus(status)) => registry.update_device(receiver_id, status),
                Ok(ControlMessage::ToneState { request_id, playing, failed }) => registry.update_tone(receiver_id, request_id, playing, failed),
                Ok(message) => debug!(?message, "ignoring unexpected client control message"),
                Err(error) => {
                    debug!(%error, "control stream closed");
                    break;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_transmission_gate_is_shared_between_clones() {
        let gate = AudioTransmissionGate::enabled();
        let worker_gate = gate.clone();
        assert!(worker_gate.is_enabled());

        gate.set_enabled(false);
        assert!(!worker_gate.is_enabled());

        worker_gate.set_enabled(true);
        assert!(gate.is_enabled());
    }

    #[test]
    fn datagram_size_selects_safe_packet_duration() {
        assert_eq!(choose_packet_duration_ms(1_920).unwrap(), 10);
        assert_eq!(choose_packet_duration_ms(960).unwrap(), 5);
        assert_eq!(choose_packet_duration_ms(384).unwrap(), 2);
        assert!(choose_packet_duration_ms(100).is_err());
    }

    fn block(timestamp: u64) -> PcmBlock {
        PcmBlock {
            timestamp,
            samples: [0; PcmBlock::SAMPLE_COUNT],
        }
    }

    fn packet_shapes(packetizer: &mut Packetizer, block: PcmBlock) -> Vec<(u64, usize)> {
        let mut packets = Vec::new();
        packetizer.for_each_packet(block, |timestamp, samples| {
            packets.push((timestamp, samples.len()));
            true
        });
        packets
    }

    #[test]
    fn four_ms_packetizer_carries_contiguous_tail_without_short_packets() {
        let mut packetizer = Packetizer::new(4);
        assert_eq!(
            packet_shapes(&mut packetizer, block(0)),
            vec![(0, FOUR_MS_PACKET_SAMPLES), (192, FOUR_MS_PACKET_SAMPLES)]
        );
        assert_eq!(
            packet_shapes(&mut packetizer, block(480)),
            vec![
                (384, FOUR_MS_PACKET_SAMPLES),
                (576, FOUR_MS_PACKET_SAMPLES),
                (768, FOUR_MS_PACKET_SAMPLES)
            ]
        );
    }

    #[test]
    fn four_ms_packetizer_discards_tail_after_capture_gap() {
        let mut packetizer = Packetizer::new(4);
        let _ = packet_shapes(&mut packetizer, block(0));
        assert_eq!(
            packet_shapes(&mut packetizer, block(960)),
            vec![
                (960, FOUR_MS_PACKET_SAMPLES),
                (1152, FOUR_MS_PACKET_SAMPLES)
            ]
        );
        assert_eq!(
            packet_shapes(&mut packetizer, block(1440)),
            vec![
                (1344, FOUR_MS_PACKET_SAMPLES),
                (1536, FOUR_MS_PACKET_SAMPLES),
                (1728, FOUR_MS_PACKET_SAMPLES)
            ]
        );
    }
}
