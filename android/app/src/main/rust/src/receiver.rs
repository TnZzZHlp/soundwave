use std::sync::Arc;

use audio_stream_protocol::{AudioPacket, StreamInfo};

use crate::{
    error::ClientError,
    jitter::{JitterBuffer, JitterOutput},
    packet::pcm_packet_layout,
    ring_buffer::PcmRingBuffer,
    state::SharedState,
};

const TARGET_JITTER_MS: usize = 50;

pub struct ReceiverPipeline {
    jitter: JitterBuffer,
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
    expected_packet_bytes: usize,
    samples_per_packet: usize,
}

impl ReceiverPipeline {
    pub fn new(
        stream: StreamInfo,
        ring: Arc<PcmRingBuffer>,
        state: Arc<SharedState>,
    ) -> Result<Self, ClientError> {
        let layout = pcm_packet_layout(stream)?;
        let target_packets = (TARGET_JITTER_MS / stream.frame_duration_ms as usize).max(1);

        Ok(Self {
            jitter: JitterBuffer::new(target_packets, layout.samples_per_channel),
            ring,
            state,
            expected_packet_bytes: layout.expected_packet_bytes,
            samples_per_packet: layout.samples_per_packet,
        })
    }

    pub fn handle_packet(&mut self, packet: AudioPacket) {
        self.state.add_received();
        if packet.payload.len() != self.expected_packet_bytes {
            self.state.add_invalid();
            return;
        }

        let lost_before = self.jitter.lost_packets();
        let late_before = self.jitter.late_packets();
        for output in self.jitter.push(packet) {
            match output {
                JitterOutput::Audio(packet) => {
                    if !self.ring.write_pcm_le(&packet.payload) {
                        self.state.add_invalid();
                    }
                }
                JitterOutput::Silence => self.ring.write_silence(self.samples_per_packet),
            }
        }
        self.state
            .add_lost(self.jitter.lost_packets().saturating_sub(lost_before));
        self.state
            .add_late(self.jitter.late_packets().saturating_sub(late_before));
    }
}
