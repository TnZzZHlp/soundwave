use std::net::AddrParseError;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("server address is invalid: {0}")]
    Address(#[from] AddrParseError),
    #[error("network endpoint failed: {0}")]
    Endpoint(#[from] std::io::Error),
    #[error("transport failed: {0}")]
    Transport(#[from] audio_stream_transport::TransportError),
    #[error("unsupported stream format: {0}")]
    UnsupportedStream(String),
    #[error("native runtime failed: {0}")]
    Runtime(String),
}
