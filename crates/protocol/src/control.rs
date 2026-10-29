use serde::{Deserialize, Serialize};

use crate::ControlError;

pub const PROTOCOL_VERSION: u16 = 8;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const CALIBRATION_FREQUENCIES: [f64; 6] = [317.0, 503.0, 797.0, 1259.0, 1999.0, 3163.0];

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
    Hello {
        protocol_version: u16,
    },
    StreamInfo(StreamInfo),
    Start,
    Stop,
    AdjustVolume {
        steps: i8,
    },
    AudioIdle {
        next_sequence: u32,
    },
    QueryDevice {
        request_id: u32,
    },
    SetVolume {
        request_id: u32,
        index: u16,
        purpose: VolumePurpose,
    },
    DeviceStatus(DeviceStatus),
    CalibrationTone {
        request_id: u32,
        enabled: bool,
    },
    ToneState {
        request_id: u32,
        playing: bool,
        failed: bool,
    },
    CalibrationSession {
        request_id: u32,
        action: CalibrationAction,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum VolumePurpose {
    User,
    Calibration,
    RestoreSpeaker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CalibrationAction {
    Begin,
    Cancel,
    Commit { index: u16 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub request_id: u32,
    pub device_id: String,
    pub name: String,
    pub volume: u16,
    pub minimum: u16,
    pub maximum: u16,
    pub fixed: bool,
    pub speaker: bool,
    pub failed: bool,
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
        for message in [
            ControlMessage::AdjustVolume { steps: -1 },
            ControlMessage::CalibrationTone {
                request_id: 7,
                enabled: true,
            },
            ControlMessage::ToneState {
                request_id: 7,
                playing: true,
                failed: false,
            },
            ControlMessage::SetVolume {
                request_id: u32::MAX,
                index: 15,
                purpose: VolumePurpose::Calibration,
            },
            ControlMessage::CalibrationSession {
                request_id: 1,
                action: CalibrationAction::Cancel,
            },
            ControlMessage::DeviceStatus(DeviceStatus {
                request_id: 1,
                device_id: "phone".into(),
                name: "Phone".into(),
                volume: 5,
                minimum: 0,
                maximum: 15,
                fixed: false,
                speaker: true,
                failed: false,
            }),
        ] {
            assert_eq!(
                ControlMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
    }
}
