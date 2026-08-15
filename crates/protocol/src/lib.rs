//! Platform-independent wire protocol for Soundwave.
//!
//! Audio is encoded manually as a fixed 12-byte network-order header followed by
//! PCM bytes. Control messages use postcard on a reliable QUIC stream.

mod audio;
mod control;
mod error;

pub use audio::{AUDIO_PACKET_HEADER_LEN, AudioPacket};
pub use control::{ControlMessage, PROTOCOL_VERSION, SampleFormat, StreamInfo};
pub use error::{ControlError, PacketError};
