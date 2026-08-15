pub mod capture;
pub mod convert;
pub mod device;

use audio_stream_audio_common::FRAME_SAMPLES_PER_CHANNEL;

/// A fixed 10 ms block of target-format PCM. Keeping samples inline avoids a
/// heap allocation on every capture frame before it enters the bounded queue.
#[derive(Debug)]
pub struct PcmBlock {
    pub timestamp: u64,
    pub samples: [i16; FRAME_SAMPLES_PER_CHANNEL * 2],
}

impl PcmBlock {
    pub const SAMPLE_COUNT: usize = FRAME_SAMPLES_PER_CHANNEL * 2;
}
