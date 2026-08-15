use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("QUIC transport error: {0}")]
    Quinn(String),
    #[error("control message is {actual} bytes; maximum is {maximum}")]
    ControlMessageTooLarge { actual: usize, maximum: usize },
    #[error("remote control stream closed")]
    ControlStreamClosed,
    #[error("QUIC peer does not advertise datagram support")]
    DatagramUnsupported,
    #[error("audio datagram is {actual} bytes; maximum supported is {maximum}")]
    DatagramTooLarge { actual: usize, maximum: usize },
    #[error("failed to create self-signed server certificate: {0}")]
    Certificate(String),
}

impl From<quinn::ConnectionError> for TransportError {
    fn from(value: quinn::ConnectionError) -> Self {
        Self::Quinn(value.to_string())
    }
}
