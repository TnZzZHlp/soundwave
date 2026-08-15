use quinn::{RecvStream, SendStream};
use tokio::io::AsyncWriteExt;

use audio_stream_protocol::ControlMessage;

use crate::TransportError;

pub const MAX_CONTROL_MESSAGE_BYTES: usize = 4 * 1024;

/// Length-delimited postcard writer over a reliable QUIC stream.
pub struct ControlWriter {
    stream: SendStream,
}

/// Length-delimited postcard reader over a reliable QUIC stream.
pub struct ControlReader {
    stream: RecvStream,
}

impl ControlWriter {
    pub fn new(stream: SendStream) -> Self {
        Self { stream }
    }

    pub async fn send(&mut self, message: &ControlMessage) -> Result<(), TransportError> {
        let encoded = message
            .encode()
            .map_err(|error| TransportError::Quinn(error.to_string()))?;
        if encoded.len() > MAX_CONTROL_MESSAGE_BYTES || encoded.len() > u16::MAX as usize {
            return Err(TransportError::ControlMessageTooLarge {
                actual: encoded.len(),
                maximum: MAX_CONTROL_MESSAGE_BYTES,
            });
        }
        self.stream
            .write_all(&(encoded.len() as u16).to_be_bytes())
            .await
            .map_err(|error| TransportError::Quinn(error.to_string()))?;
        self.stream
            .write_all(&encoded)
            .await
            .map_err(|error| TransportError::Quinn(error.to_string()))?;
        self.stream
            .flush()
            .await
            .map_err(|error| TransportError::Quinn(error.to_string()))
    }

    pub fn finish(&mut self) -> Result<(), TransportError> {
        self.stream
            .finish()
            .map_err(|error| TransportError::Quinn(error.to_string()))
    }
}

impl ControlReader {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream }
    }

    pub async fn receive(&mut self) -> Result<ControlMessage, TransportError> {
        let mut length = [0_u8; 2];
        self.stream
            .read_exact(&mut length)
            .await
            .map_err(|error| TransportError::Quinn(error.to_string()))?;
        let length = u16::from_be_bytes(length) as usize;
        if length > MAX_CONTROL_MESSAGE_BYTES {
            return Err(TransportError::ControlMessageTooLarge {
                actual: length,
                maximum: MAX_CONTROL_MESSAGE_BYTES,
            });
        }

        let mut encoded = vec![0_u8; length];
        self.stream
            .read_exact(&mut encoded)
            .await
            .map_err(|error| TransportError::Quinn(error.to_string()))?;
        ControlMessage::decode(&encoded).map_err(|error| TransportError::Quinn(error.to_string()))
    }
}
