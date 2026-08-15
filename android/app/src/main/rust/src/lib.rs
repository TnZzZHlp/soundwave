#![deny(unsafe_op_in_unsafe_fn)]

//! Android native streaming core. Kotlin owns Android lifecycle and AudioTrack;
//! this crate owns QUIC, packet validation, jitter handling, and the PCM ring.

mod client;
mod error;
mod jitter;
mod jni;
mod packet;
mod player;
mod receiver;
mod ring_buffer;
mod state;
