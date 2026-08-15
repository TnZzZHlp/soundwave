use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use audio_stream_audio_common::{CHANNELS, SAMPLE_RATE};
use audio_stream_protocol::{
    AudioPacket, ControlMessage, PROTOCOL_VERSION, SampleFormat, StreamInfo,
};
use audio_stream_transport::{ControlReader, ControlWriter, DatagramSender, TransportError};
use bytes::Bytes;
use crossbeam_channel::Receiver;
use quinn::Connection;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::audio::PcmBlock;

pub const FRAME_QUEUE_CAPACITY: usize = 8;

#[derive(Default)]
pub struct StreamCounters {
    pub packets_sent: AtomicU64,
    pub packets_dropped: AtomicU64,
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

pub fn spawn_datagram_sender(
    connection: Connection,
    frames: Receiver<PcmBlock>,
    frame_duration_ms: u8,
    stop: CancellationToken,
    counters: Arc<StreamCounters>,
) -> std::io::Result<std::thread::JoinHandle<Result<(), TransportError>>> {
    std::thread::Builder::new()
        .name("quic-audio-send".to_owned())
        .spawn(move || {
            let sender = match DatagramSender::new(connection) {
                Ok(sender) => sender,
                Err(error) => {
                    stop.cancel();
                    return Err(error);
                }
            };
            let mut packetizer = Packetizer::new(frame_duration_ms);
            let result = loop {
                if stop.is_cancelled() {
                    break Ok(());
                }
                match frames.recv_timeout(Duration::from_millis(100)) {
                    Ok(block) => packetizer.send_block(&sender, block, &counters),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break Ok(()),
                }?;
            };
            if result.is_err() {
                stop.cancel();
            }
            result
        })
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

struct Packetizer {
    sequence: u32,
    samples_per_packet: usize,
}

impl Packetizer {
    fn new(frame_duration_ms: u8) -> Self {
        Self {
            sequence: 0,
            samples_per_packet: samples_per_packet(frame_duration_ms),
        }
    }

    fn send_block(
        &mut self,
        sender: &DatagramSender,
        block: PcmBlock,
        counters: &StreamCounters,
    ) -> Result<(), TransportError> {
        for (index, samples) in block.samples.chunks(self.samples_per_packet).enumerate() {
            let mut payload = Vec::with_capacity(std::mem::size_of_val(samples));
            for sample in samples {
                payload.extend_from_slice(&sample.to_le_bytes());
            }
            let samples_before = index * self.samples_per_packet;
            let timestamp = block.timestamp + (samples_before / CHANNELS as usize) as u64;
            let packet = AudioPacket::new(self.sequence, timestamp, Bytes::from(payload));
            self.sequence = self.sequence.wrapping_add(1);

            match sender.send(packet) {
                Ok(()) => {
                    counters.packets_sent.fetch_add(1, Ordering::Relaxed);
                }
                // A full local QUIC datagram queue is treated as loss. It is
                // preferable to lose this old frame than to buffer it and grow latency.
                Err(TransportError::Quinn(_)) => {
                    counters.packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn samples_per_packet(frame_duration_ms: u8) -> usize {
    (SAMPLE_RATE as usize * frame_duration_ms as usize / 1_000) * CHANNELS as usize
}

pub async fn process_control_messages(
    reader: &mut ControlReader,
    writer: &mut ControlWriter,
    connection: &Connection,
    stop: &CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = connection.closed() => break,
            message = reader.receive() => match message {
                Ok(ControlMessage::Stop) => {
                    debug!("client requested stream stop");
                    break;
                }
                Ok(ControlMessage::Ping { timestamp }) => {
                    if let Err(error) = writer.send(&ControlMessage::Pong { timestamp }).await {
                        warn!(%error, "failed to send control pong");
                        break;
                    }
                }
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
    fn datagram_size_selects_safe_packet_duration() {
        assert_eq!(choose_packet_duration_ms(1_920).unwrap(), 10);
        assert_eq!(choose_packet_duration_ms(960).unwrap(), 5);
        assert_eq!(choose_packet_duration_ms(384).unwrap(), 2);
        assert!(choose_packet_duration_ms(100).is_err());
    }
}
