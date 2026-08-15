use thiserror::Error;

#[derive(Debug, Error)]
pub enum PacketError {
    #[error("audio packet is {actual} bytes, but needs at least {minimum} bytes")]
    TooShort { actual: usize, minimum: usize },
    #[error("audio packet header is malformed")]
    Malformed,
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("failed to encode control message: {0}")]
    Encode(postcard::Error),
    #[error("failed to decode control message: {0}")]
    Decode(postcard::Error),
}
