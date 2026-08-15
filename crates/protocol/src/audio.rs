use bytes::{BufMut, Bytes, BytesMut};

use crate::PacketError;

/// Bytes occupied by the fixed audio datagram header.
pub const AUDIO_PACKET_HEADER_LEN: usize = 12;

/// A single real-time audio datagram.
///
/// `sequence` increments once per PCM frame. `timestamp` is the source sample
/// position, not wall-clock time. Header integers are encoded in network byte
/// order (big endian); PCM payload remains little-endian signed 16-bit samples.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacket {
    pub sequence: u32,
    pub timestamp: u64,
    pub payload: Bytes,
}

impl AudioPacket {
    pub fn new(sequence: u32, timestamp: u64, payload: impl Into<Bytes>) -> Self {
        Self {
            sequence,
            timestamp,
            payload: payload.into(),
        }
    }

    /// Serializes this packet into the exact wire format used in QUIC datagrams.
    pub fn encode(&self) -> Bytes {
        let mut encoded = BytesMut::with_capacity(AUDIO_PACKET_HEADER_LEN + self.payload.len());
        encoded.put_u32(self.sequence);
        encoded.put_u64(self.timestamp);
        encoded.extend_from_slice(&self.payload);
        encoded.freeze()
    }

    /// Parses a packet from its wire format.
    pub fn decode(encoded: impl AsRef<[u8]>) -> Result<Self, PacketError> {
        let encoded = encoded.as_ref();
        if encoded.len() < AUDIO_PACKET_HEADER_LEN {
            return Err(PacketError::TooShort {
                actual: encoded.len(),
                minimum: AUDIO_PACKET_HEADER_LEN,
            });
        }

        let sequence = u32::from_be_bytes(
            encoded[0..4]
                .try_into()
                .map_err(|_| PacketError::Malformed)?,
        );
        let timestamp = u64::from_be_bytes(
            encoded[4..12]
                .try_into()
                .map_err(|_| PacketError::Malformed)?,
        );

        Ok(Self {
            sequence,
            timestamp,
            payload: Bytes::copy_from_slice(&encoded[AUDIO_PACKET_HEADER_LEN..]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip_is_lossless() {
        let packet = AudioPacket::new(
            0x0102_0304,
            0x0102_0304_0506_0708,
            vec![0x34, 0x12, 0x78, 0x56],
        );
        let encoded = packet.encode();

        assert_eq!(
            encoded.as_ref(),
            &[
                0x01, 0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x34, 0x12,
                0x78, 0x56
            ]
        );
        assert_eq!(AudioPacket::decode(encoded).unwrap(), packet);
    }

    #[test]
    fn too_short_packet_is_rejected() {
        assert!(matches!(
            AudioPacket::decode([1, 2, 3]),
            Err(PacketError::TooShort { .. })
        ));
    }
}
