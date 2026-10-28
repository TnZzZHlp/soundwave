use serde::{Deserialize, Serialize};

use crate::ControlError;

pub const PROTOCOL_VERSION: u16 = 6;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;

/// Audio sample representation advertised before streaming begins.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SampleFormat {
    I16Le,
}

/// Immutable format information for an audio stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u8,
    pub sample_format: SampleFormat,
    pub frame_duration_ms: u8,
}

/// Control messages travel over a reliable, length-delimited QUIC bidirectional stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    Hello { protocol_version: u16 },
    StreamInfo(StreamInfo),
    Start,
    Stop,
    AdjustVolume { steps: i8 },
    AudioIdle { next_sequence: u32 },
}

impl ControlMessage {
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        postcard::to_allocvec(self).map_err(ControlError::Encode)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, ControlError> {
        postcard::from_bytes(encoded).map_err(ControlError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trip_is_lossless() {
        let original = ControlMessage::StreamInfo(StreamInfo {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::I16Le,
            frame_duration_ms: 10,
        });

        assert_eq!(
            ControlMessage::decode(&original.encode().unwrap()).unwrap(),
            original
        );
        let volume = ControlMessage::AdjustVolume { steps: -1 };
        assert_eq!(
            ControlMessage::decode(&volume.encode().unwrap()).unwrap(),
            volume
        );
    }
}
