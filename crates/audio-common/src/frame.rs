use thiserror::Error;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const FRAME_DURATION_MS: u8 = 10;
pub const FRAME_SAMPLES_PER_CHANNEL: usize =
    (SAMPLE_RATE as usize * FRAME_DURATION_MS as usize) / 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_duration_ms: u8,
}

impl AudioFormat {
    pub const PCM_48K_STEREO_10MS: Self = Self {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        frame_duration_ms: FRAME_DURATION_MS,
    };

    pub fn samples_per_channel(self) -> Result<usize, AudioFormatError> {
        if self.sample_rate == 0 || self.channels == 0 || self.frame_duration_ms == 0 {
            return Err(AudioFormatError::Invalid);
        }

        let samples = (self.sample_rate as usize)
            .checked_mul(self.frame_duration_ms as usize)
            .ok_or(AudioFormatError::Invalid)?;
        if samples % 1_000 != 0 {
            return Err(AudioFormatError::FrameDoesNotAlign);
        }
        Ok(samples / 1_000)
    }

    pub fn samples_per_frame(self) -> Result<usize, AudioFormatError> {
        self.samples_per_channel()?
            .checked_mul(self.channels as usize)
            .ok_or(AudioFormatError::Invalid)
    }

    pub fn bytes_per_frame(self) -> Result<usize, AudioFormatError> {
        self.samples_per_frame()?
            .checked_mul(std::mem::size_of::<i16>())
            .ok_or(AudioFormatError::Invalid)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmFrame {
    pub timestamp: u64,
    pub samples: Vec<i16>,
}

impl PcmFrame {
    pub fn new(
        timestamp: u64,
        samples: Vec<i16>,
        format: AudioFormat,
    ) -> Result<Self, AudioFormatError> {
        let expected = format.samples_per_frame()?;
        if samples.len() != expected {
            return Err(AudioFormatError::WrongFrameSampleCount {
                actual: samples.len(),
                expected,
            });
        }
        Ok(Self { timestamp, samples })
    }
}

#[derive(Debug, Error)]
pub enum AudioFormatError {
    #[error("audio format contains a zero or overflowing field")]
    Invalid,
    #[error("frame duration does not map to an integral number of samples")]
    FrameDoesNotAlign,
    #[error("frame has {actual} samples; expected {expected}")]
    WrongFrameSampleCount { actual: usize, expected: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v01_frame_size_is_1920_bytes() {
        assert_eq!(
            AudioFormat::PCM_48K_STEREO_10MS
                .samples_per_channel()
                .unwrap(),
            480
        );
        assert_eq!(
            AudioFormat::PCM_48K_STEREO_10MS
                .samples_per_frame()
                .unwrap(),
            960
        );
        assert_eq!(
            AudioFormat::PCM_48K_STEREO_10MS.bytes_per_frame().unwrap(),
            1_920
        );
    }
}
