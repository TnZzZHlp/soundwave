//! Shared, allocation-conscious PCM utilities.

mod convert;
mod frame;
mod ring_buffer;

pub use convert::{f32_to_i16, interleaved_f32_to_i16};
pub use frame::{
    AudioFormat, AudioFormatError, CHANNELS, FRAME_DURATION_MS, FRAME_SAMPLES_PER_CHANNEL,
    PcmFrame, SAMPLE_RATE,
};
pub use ring_buffer::{PcmConsumer, PcmProducer, pcm_ring_buffer};
