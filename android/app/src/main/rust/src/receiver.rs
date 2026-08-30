use std::sync::Arc;

use audio_stream_protocol::{AudioPacket, StreamInfo};

use crate::{
    error::ClientError,
    jitter::{JitterBuffer, JitterOutput},
    packet::pcm_packet_layout,
    ring_buffer::PcmRingBuffer,
    state::SharedState,
};

const INITIAL_JITTER_BUFFER_MS: usize = 20;
const JITTER_BUFFER_STEP_MS: usize = 10;
const ADAPTATION_COOLDOWN_MS: usize = 1_000;

pub struct ReceiverPipeline {
    jitter: JitterBuffer,
    ring: Arc<PcmRingBuffer>,
    state: Arc<SharedState>,
    expected_packet_bytes: usize,
    samples_per_packet: usize,
    frame_duration_ms: usize,
    adjustment_packets: usize,
    maximum_target_packets: usize,
    adaptation_cooldown_packets: usize,
    cooldown_after_adjustment_packets: usize,
    last_underruns: u64,
}

impl ReceiverPipeline {
    pub fn new(
        stream: StreamInfo,
        ring: Arc<PcmRingBuffer>,
        state: Arc<SharedState>,
    ) -> Result<Self, ClientError> {
        let layout = pcm_packet_layout(stream)?;
        let target_packets =
            packets_for_duration(INITIAL_JITTER_BUFFER_MS, stream.frame_duration_ms)?;
        let adjustment_packets =
            packets_for_duration(JITTER_BUFFER_STEP_MS, stream.frame_duration_ms)?;
        let maximum_target_packets =
            packets_for_duration(crate::MAX_JITTER_BUFFER_MS, stream.frame_duration_ms)?;
        let cooldown_after_adjustment_packets =
            packets_for_duration(ADAPTATION_COOLDOWN_MS, stream.frame_duration_ms)?;
        let frame_duration_ms = usize::from(stream.frame_duration_ms);
        state.set_jitter_buffer_ms(
            u64::try_from(frame_duration_ms * target_packets).unwrap_or(u64::MAX),
        );
        let last_underruns = ring.underruns();

        Ok(Self {
            jitter: JitterBuffer::new(target_packets, layout.samples_per_channel),
            ring,
            state,
            expected_packet_bytes: layout.expected_packet_bytes,
            samples_per_packet: layout.samples_per_packet,
            frame_duration_ms,
            adjustment_packets,
            maximum_target_packets,
            adaptation_cooldown_packets: cooldown_after_adjustment_packets,
            cooldown_after_adjustment_packets,
            last_underruns,
        })
    }

    pub fn handle_packet(&mut self, packet: AudioPacket) {
        self.state.add_received();
        if packet.payload.len() != self.expected_packet_bytes {
            self.state.add_invalid();
            return;
        }

        let underruns = self.ring.underruns();
        let underrun_detected = underruns > self.last_underruns;
        self.last_underruns = underruns;
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
        let late_packets = self.jitter.late_packets();
        self.state
            .add_late(late_packets.saturating_sub(late_before));
        self.adjust_jitter_after_stutter(underrun_detected || late_packets > late_before);
    }

    fn adjust_jitter_after_stutter(&mut self, stutter_detected: bool) {
        if self.adaptation_cooldown_packets > 0 {
            self.adaptation_cooldown_packets -= 1;
        } else if stutter_detected
            && self.jitter.is_started()
            && self
                .jitter
                .increase_target(self.adjustment_packets, self.maximum_target_packets)
        {
            self.adaptation_cooldown_packets = self.cooldown_after_adjustment_packets;
            self.state.set_jitter_buffer_ms(
                u64::try_from(self.frame_duration_ms * self.jitter.target_packets())
                    .unwrap_or(u64::MAX),
            );
        }
    }
}

fn packets_for_duration(duration_ms: usize, frame_duration_ms: u8) -> Result<usize, ClientError> {
    let frame_duration_ms = usize::from(frame_duration_ms);
    if frame_duration_ms == 0 {
        return Err(ClientError::UnsupportedStream(
            "frame duration must be greater than zero".to_owned(),
        ));
    }
    Ok(duration_ms.div_ceil(frame_duration_ms).max(1))
}

#[cfg(test)]
mod tests {
    use audio_stream_protocol::{SampleFormat, StreamInfo};
    use bytes::Bytes;

    use super::*;

    fn stream() -> StreamInfo {
        StreamInfo {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::I16Le,
            frame_duration_ms: 5,
        }
    }

    fn packet(sequence: u32) -> AudioPacket {
        AudioPacket::new(
            sequence,
            u64::from(sequence) * 240,
            Bytes::from(vec![0; 960]),
        )
    }

    #[test]
    fn jitter_target_rounds_to_whole_packets() {
        assert_eq!(packets_for_duration(20, 5).unwrap(), 4);
        assert_eq!(packets_for_duration(21, 5).unwrap(), 5);
        assert!(packets_for_duration(20, 0).is_err());
    }

    #[test]
    fn underrun_increases_jitter_target_and_rebuffers() {
        let ring = Arc::new(PcmRingBuffer::new(9_600));
        let state = Arc::new(SharedState::default());
        let mut pipeline = ReceiverPipeline::new(stream(), ring.clone(), state.clone()).unwrap();
        pipeline.adaptation_cooldown_packets = 0;

        for sequence in 0..4 {
            pipeline.handle_packet(packet(sequence));
        }
        assert_eq!(state.jitter_buffer_ms(), 20);
        ring.clear();
        let mut output = [0_i16; 1];
        ring.read_into(&mut output);

        pipeline.handle_packet(packet(4));

        assert_eq!(state.jitter_buffer_ms(), 30);
        assert_eq!(pipeline.jitter.target_packets(), 6);
        assert_eq!(ring.available_samples(), 480);
    }
}
