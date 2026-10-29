#![deny(unsafe_op_in_unsafe_fn)]

//! Android native streaming core. Kotlin owns Android lifecycle and `AudioTrack`;
//! this crate owns QUIC, packet validation, jitter handling, and the PCM ring.

mod client;
mod device_control;
mod error;
mod jitter;
mod jni;
mod receiver;
mod ring_buffer;
mod state;

const MAX_JITTER_BUFFER_MS: usize = 100;
