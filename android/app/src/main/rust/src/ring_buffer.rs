use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

use audio_stream_audio_common::{CHANNELS, SAMPLE_RATE};

/// 400 ms cap: enough headroom to ride out `WiFi` stall bursts, while making
/// latency growth impossible even if the audio device temporarily stalls.
/// Ring capacity only bounds stall tolerance; steady-state playback latency
/// is set by the jitter target, not by this constant.
pub const PCM_RING_CAPACITY_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize * 2 / 5;

/// A lock-free bounded PCM ring. Its public methods only need `&self`, so the
/// network producer and Kotlin's `AudioTrack` writer never contend on a mutex.
pub struct PcmRingBuffer {
    queue: ArrayQueue<i16>,
    underruns: AtomicU64,
    overwritten_samples: AtomicU64,
}

impl PcmRingBuffer {
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity_samples),
            underruns: AtomicU64::new(0),
            overwritten_samples: AtomicU64::new(0),
        }
    }

    pub fn write_pcm_le(&self, pcm: &[u8]) -> bool {
        if pcm.len() % std::mem::size_of::<i16>() != 0 {
            return false;
        }
        for bytes in pcm.chunks_exact(std::mem::size_of::<i16>()) {
            self.push_latest(i16::from_le_bytes([bytes[0], bytes[1]]));
        }
        true
    }

    pub fn write_silence(&self, samples: usize) {
        for _ in 0..samples {
            self.push_latest(0);
        }
    }

    /// This is the entire native audio callback path: atomically pop or fill
    /// silence. It allocates nothing and does no I/O or locking.
    pub fn read_into(&self, output: &mut [i16]) -> usize {
        let mut actual = 0;
        for sample in &mut *output {
            if let Some(value) = self.queue.pop() {
                *sample = value;
                actual += 1;
            } else {
                *sample = 0;
            }
        }
        if actual != output.len() {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        actual
    }

    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
    }

    pub fn available_samples(&self) -> usize {
        self.queue.len()
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overwritten_samples(&self) -> u64 {
        self.overwritten_samples.load(Ordering::Relaxed)
    }

    fn push_latest(&self, sample: i16) {
        if let Err(sample) = self.queue.push(sample) {
            // The only producer is the receiver task. Removing before retrying
            // deliberately discards the oldest audible data, not the newest.
            if self.queue.pop().is_some() {
                self.overwritten_samples.fetch_add(1, Ordering::Relaxed);
            }
            if self.queue.push(sample).is_err() {
                self.overwritten_samples.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_overwrites_oldest_and_silences_underrun() {
        let ring = PcmRingBuffer::new(3);
        assert!(ring.write_pcm_le(&[1, 0, 2, 0, 3, 0, 4, 0]));
        let mut output = [99_i16; 5];
        assert_eq!(ring.read_into(&mut output), 3);
        assert_eq!(output, [2, 3, 4, 0, 0]);
        assert_eq!(ring.underruns(), 1);
    }
}
