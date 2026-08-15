use serde::{Deserialize, Serialize};

use crate::ControlError;

pub const PROTOCOL_VERSION: u16 = 1;

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
    Ping { timestamp: u64 },
    Pong { timestamp: u64 },
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
    }
}
