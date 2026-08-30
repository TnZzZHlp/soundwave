use std::collections::BTreeMap;

use audio_stream_protocol::AudioPacket;

/// One item ready to enter the PCM ring. Lost packets intentionally become
/// silence; playback never waits indefinitely for an old datagram.
#[derive(Debug)]
pub enum JitterOutput {
    Audio(AudioPacket),
    Silence,
}

pub struct JitterBuffer {
    expected_sequence: Option<u32>,
    expected_timestamp: Option<u64>,
    packets: BTreeMap<u32, AudioPacket>,
    target_packets: usize,
    max_packets: usize,
    frame_samples_per_channel: u64,
    started: bool,
    lost_packets: u64,
    late_packets: u64,
}

impl JitterBuffer {
    pub fn new(target_packets: usize, frame_samples_per_channel: u64) -> Self {
        let target_packets = target_packets.max(1);
        Self {
            expected_sequence: None,
            expected_timestamp: None,
            packets: BTreeMap::new(),
            target_packets,
            max_packets: target_packets.saturating_mul(3).max(4),
            frame_samples_per_channel,
            started: false,
            lost_packets: 0,
            late_packets: 0,
        }
    }

    pub fn push(&mut self, packet: AudioPacket) -> Vec<JitterOutput> {
        let expected = self.expected_sequence.get_or_insert(packet.sequence);
        if self.packets.contains_key(&packet.sequence) {
            // Ignore a duplicate while the original is still buffered so it
            // does not corrupt the late-packet diagnostic counter.
            return Vec::new();
        }
        if sequence_is_before(packet.sequence, *expected) {
            // A duplicate or reordered packet arriving after playback passed
            // this sequence is too late to play.
            self.late_packets = self.late_packets.saturating_add(1);
            return Vec::new();
        }
        self.packets.insert(packet.sequence, packet);
        self.drain_ready()
    }

    pub const fn lost_packets(&self) -> u64 {
        self.lost_packets
    }

    pub const fn late_packets(&self) -> u64 {
        self.late_packets
    }

    pub const fn target_packets(&self) -> usize {
        self.target_packets
    }

    pub const fn is_started(&self) -> bool {
        self.started
    }

    pub fn increase_target(&mut self, additional_packets: usize, maximum_packets: usize) -> bool {
        let maximum_packets = maximum_packets.max(1);
        if self.target_packets >= maximum_packets {
            return false;
        }
        self.target_packets = self
            .target_packets
            .saturating_add(additional_packets.max(1))
            .min(maximum_packets);
        self.max_packets = self.target_packets.saturating_mul(3).max(4);
        self.started = false;
        true
    }

    fn drain_ready(&mut self) -> Vec<JitterOutput> {
        if !self.started {
            if self.packets.len() < self.target_packets {
                return Vec::new();
            }
            self.started = true;
        }

        let mut output = Vec::new();
        while let Some(expected) = self.expected_sequence {
            if let Some(packet) = self.packets.remove(&expected) {
                self.expected_sequence = Some(expected.wrapping_add(1));
                self.expected_timestamp = Some(
                    packet
                        .timestamp
                        .wrapping_add(self.frame_samples_per_channel),
                );
                output.push(JitterOutput::Audio(packet));
                continue;
            }

            let Some(next_sequence) = self.next_buffered_sequence(expected) else {
                break;
            };
            let missing = next_sequence.wrapping_sub(expected);
            if u64::from(missing) > self.max_packets as u64 {
                // An old packet cannot arrive within the bounded jitter window;
                // skip directly to the nearest buffered packet instead of
                // allocating one silence item per missing sequence.
                self.lost_packets = self.lost_packets.saturating_add(u64::from(missing));
                self.expected_sequence = Some(next_sequence);
                self.expected_timestamp = self
                    .packets
                    .get(&next_sequence)
                    .map(|packet| packet.timestamp);
                continue;
            }
            if self.packets.len() < self.target_packets && self.packets.len() < self.max_packets {
                break;
            }

            let timestamp = self.expected_timestamp.unwrap_or(0);
            self.expected_timestamp = Some(timestamp.wrapping_add(self.frame_samples_per_channel));
            self.expected_sequence = Some(expected.wrapping_add(1));
            self.lost_packets = self.lost_packets.saturating_add(1);
            output.push(JitterOutput::Silence);
        }
        output
    }

    fn next_buffered_sequence(&self, expected: u32) -> Option<u32> {
        self.packets
            .keys()
            .copied()
            .filter(|sequence| sequence_is_after(*sequence, expected))
            .min_by_key(|sequence| sequence.wrapping_sub(expected))
    }
}

const fn sequence_is_before(sequence: u32, reference: u32) -> bool {
    let distance = sequence.wrapping_sub(reference);
    distance != 0 && distance >= (1_u32 << 31)
}

const fn sequence_is_after(sequence: u32, reference: u32) -> bool {
    let distance = sequence.wrapping_sub(reference);
    distance != 0 && distance < (1_u32 << 31)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn packet(sequence: u32) -> AudioPacket {
        AudioPacket {
            sequence,
            timestamp: u64::from(sequence) * 480,
            payload: Bytes::from_static(&[0, 0]),
        }
    }

    fn sequences(outputs: Vec<JitterOutput>) -> Vec<u32> {
        outputs
            .into_iter()
            .map(|output| match output {
                JitterOutput::Audio(packet) => packet.sequence,
                JitterOutput::Silence => 0,
            })
            .collect()
    }

    #[test]
    fn duplicate_copy_while_buffered_is_ignored() {
        let mut jitter = JitterBuffer::new(2, 480);
        let _ = jitter.push(packet(1));
        let _ = jitter.push(packet(1)); // duplicate, still buffered
        assert_eq!(jitter.late_packets(), 0);
        assert_eq!(sequences(jitter.push(packet(2))), vec![1, 2]);
        assert_eq!(jitter.late_packets(), 0);
        assert_eq!(jitter.lost_packets(), 0);
    }

    #[test]
    fn handles_minor_reordering() {
        let mut jitter = JitterBuffer::new(2, 480);
        assert_eq!(sequences(jitter.push(packet(1))), Vec::<u32>::new());
        assert_eq!(sequences(jitter.push(packet(2))), vec![1, 2]);
        assert_eq!(sequences(jitter.push(packet(4))), Vec::<u32>::new());
        assert_eq!(sequences(jitter.push(packet(3))), vec![3, 4]);
        assert_eq!(jitter.lost_packets(), 0);
    }

    #[test]
    fn increases_target_until_cap() {
        let mut jitter = JitterBuffer::new(2, 480);
        assert_eq!(jitter.target_packets(), 2);
        assert!(jitter.increase_target(2, 5));
        assert_eq!(jitter.target_packets(), 4);
        assert!(!jitter.is_started());
        assert!(jitter.increase_target(2, 5));
        assert_eq!(jitter.target_packets(), 5);
        assert!(!jitter.increase_target(2, 5));
    }

    #[test]
    fn converts_missing_packet_to_silence() {
        let mut jitter = JitterBuffer::new(2, 480);
        let _ = jitter.push(packet(1));
        assert_eq!(sequences(jitter.push(packet(2))), vec![1, 2]);
        let _ = jitter.push(packet(4));
        assert_eq!(sequences(jitter.push(packet(5))), vec![0, 4, 5]);
        assert_eq!(jitter.lost_packets(), 1);
    }

    #[test]
    fn resynchronizes_after_far_gap_without_unbounded_silence() {
        let mut jitter = JitterBuffer::new(2, 480);
        let _ = jitter.push(packet(0));
        assert_eq!(sequences(jitter.push(packet(1))), vec![0, 1]);

        let far_sequence = 10_000;
        let output = jitter.push(packet(far_sequence));
        assert_eq!(sequences(output), vec![far_sequence]);
        let output = jitter.push(packet(far_sequence + 1));
        assert_eq!(sequences(output), vec![far_sequence + 1]);
        assert_eq!(jitter.lost_packets(), u64::from(far_sequence - 2));

        assert_eq!(
            sequences(jitter.push(packet(far_sequence + 2))),
            vec![far_sequence + 2]
        );
    }
}
