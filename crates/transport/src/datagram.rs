use bytes::Bytes;
use quinn::Connection;

use audio_stream_protocol::AudioPacket;

use crate::TransportError;

pub struct DatagramSender {
    connection: Connection,
}

pub struct DatagramReceiver {
    connection: Connection,
}

impl DatagramSender {
    pub fn new(connection: Connection) -> Result<Self, TransportError> {
        if connection.max_datagram_size().is_none() {
            return Err(TransportError::DatagramUnsupported);
        }
        Ok(Self { connection })
    }

    pub fn max_payload_len(&self) -> usize {
        self.connection
            .max_datagram_size()
            .unwrap_or_default()
            .saturating_sub(audio_stream_protocol::AUDIO_PACKET_HEADER_LEN)
    }

    /// Sends one packet. A full QUIC send buffer is treated as a transient loss,
    /// not a reason to queue old real-time audio.
    pub fn send(&self, packet: AudioPacket) -> Result<(), TransportError> {
        let encoded = packet.encode();
        let allowed = self
            .connection
            .max_datagram_size()
            .ok_or(TransportError::DatagramUnsupported)?;
        if encoded.len() > allowed {
            return Err(TransportError::DatagramTooLarge {
                actual: encoded.len(),
                maximum: allowed,
            });
        }
        self.connection
            .send_datagram(encoded)
            .map_err(|error| TransportError::Quinn(error.to_string()))
    }
}

impl DatagramReceiver {
    pub fn new(connection: Connection) -> Result<Self, TransportError> {
        if connection.max_datagram_size().is_none() {
            return Err(TransportError::DatagramUnsupported);
        }
        Ok(Self { connection })
    }

    pub async fn receive(&self) -> Result<AudioPacket, TransportError> {
        let encoded: Bytes = self
            .connection
            .read_datagram()
            .await
            .map_err(TransportError::from)?;
        AudioPacket::decode(encoded).map_err(|error| TransportError::Quinn(error.to_string()))
    }
}
