use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;

use audio_stream_protocol::{CHANNELS, SAMPLE_RATE};

/// Supports the largest automatic jitter target. This is only a hard
/// stale-audio ceiling; normal occupancy follows the adaptive target.
pub const PCM_RING_CAPACITY_SAMPLES: usize =
    SAMPLE_RATE as usize * CHANNELS as usize * crate::MAX_JITTER_BUFFER_MS / 1_000;

/// A lock-free bounded PCM ring. Its public methods only need `&self`, so the
/// network producer and Kotlin's `AudioTrack` writer never contend on a mutex.
pub struct PcmRingBuffer {
    queue: ArrayQueue<i16>,
    underruns: AtomicU64,
    playback_target_samples: AtomicUsize,
    buffering: AtomicBool,
}

impl PcmRingBuffer {
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity_samples),
            underruns: AtomicU64::new(0),
            playback_target_samples: AtomicUsize::new(0),
            buffering: AtomicBool::new(false),
        }
    }

    pub fn write_pcm_le(&self, pcm: &[u8]) {
        for bytes in pcm.chunks_exact(std::mem::size_of::<i16>()) {
            self.push_latest(i16::from_le_bytes([bytes[0], bytes[1]]));
        }
    }

    pub fn write_silence(&self, samples: usize) {
        for _ in 0..samples {
            self.push_latest(0);
        }
    }

    pub fn set_playback_target(&self, samples: usize) {
        self.playback_target_samples
            .store(samples.min(self.queue.capacity()), Ordering::Release);
    }

    pub fn begin_buffering(&self) {
        self.buffering.store(true, Ordering::Release);
    }

    /// This is the entire native audio callback path: atomically pop or fill
    /// silence. It allocates nothing and does no I/O or locking.
    pub fn read_into(&self, output: &mut [i16]) -> usize {
        if self.buffering.load(Ordering::Acquire) {
            let required = self
                .playback_target_samples
                .load(Ordering::Acquire)
                .max(output.len());
            if self.queue.len() < required {
                output.fill(0);
                return 0;
            }
            self.buffering.store(false, Ordering::Release);
        }
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
            if self.playback_target_samples.load(Ordering::Acquire) > 0 {
                self.begin_buffering();
            }
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

    fn push_latest(&self, sample: i16) {
        if let Err(sample) = self.queue.push(sample) {
            // The only producer is the receiver task. Removing before retrying
            // deliberately discards the oldest audible data, not the newest.
            let _ = self.queue.pop();
            let _ = self.queue.push(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_waits_for_prefill_without_counting_startup_underruns() {
        let ring = PcmRingBuffer::new(8);
        ring.set_playback_target(4);
        ring.begin_buffering();
        let mut output = [0; 2];
        ring.write_pcm_le(&[1, 0, 2, 0]);
        assert_eq!(ring.read_into(&mut output), 0);
        assert_eq!(ring.available_samples(), 2);
        assert_eq!(ring.underruns(), 0);
        ring.write_pcm_le(&[3, 0, 4, 0]);
        assert_eq!(ring.read_into(&mut output), 2);
        assert_eq!(output, [1, 2]);
    }

    #[test]
    fn playback_rebuffers_after_underrun_without_repeated_empty_reads() {
        let ring = PcmRingBuffer::new(8);
        ring.set_playback_target(4);
        let mut output = [0; 2];
        assert_eq!(ring.read_into(&mut output), 0);
        assert_eq!(ring.underruns(), 1);
        ring.write_pcm_le(&[1, 0, 2, 0]);
        assert_eq!(ring.read_into(&mut output), 0);
        assert_eq!(ring.underruns(), 1);
        ring.write_pcm_le(&[3, 0, 4, 0]);
        assert_eq!(ring.read_into(&mut output), 2);
        assert_eq!(output, [1, 2]);
    }

    #[test]
    fn ring_overwrites_oldest_and_silences_underrun() {
        let ring = PcmRingBuffer::new(3);
        ring.write_pcm_le(&[1, 0, 2, 0, 3, 0, 4, 0]);
        let mut output = [99_i16; 5];
        assert_eq!(ring.read_into(&mut output), 3);
        assert_eq!(output, [2, 3, 4, 0, 0]);
        assert_eq!(ring.underruns(), 1);
    }
}
