pub mod capture;
pub mod convert;
pub mod device;
pub mod microphone;
pub mod transmission;

use audio_stream_protocol::{CHANNELS, SAMPLE_RATE};

const PCM_BLOCK_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize / 100;

/// A fixed 10 ms block of target-format PCM. Keeping samples inline avoids a
/// heap allocation on every capture frame before it enters the bounded queue.
#[derive(Debug)]
pub struct PcmBlock {
    pub timestamp: u64,
    pub samples: [i16; PCM_BLOCK_SAMPLES],
}

impl PcmBlock {
    pub const SAMPLE_COUNT: usize = PCM_BLOCK_SAMPLES;
}
