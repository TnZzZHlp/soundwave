//! Platform-independent wire protocol for Soundwave.
//!
//! Audio is encoded manually as a fixed 12-byte network-order header followed by
//! PCM bytes. Control messages use postcard on a reliable QUIC stream.

mod audio;
mod control;
mod error;

pub use audio::{AUDIO_PACKET_HEADER_LEN, AudioPacket};
pub use control::{
    CALIBRATION_FREQUENCIES, CHANNELS, CalibrationAction, ControlMessage, DeviceStatus,
    PROTOCOL_VERSION, SAMPLE_RATE, SampleFormat, StreamInfo, VolumePurpose,
};
pub use error::{ControlError, PacketError};
