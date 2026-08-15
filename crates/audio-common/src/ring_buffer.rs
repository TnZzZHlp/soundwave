use rtrb::{Consumer, Producer, RingBuffer};

/// Single-producer / single-consumer PCM writer.
pub struct PcmProducer {
    inner: Producer<i16>,
}

/// Single-producer / single-consumer PCM reader. Reading fewer samples than
/// requested fills the remainder with silence, which keeps an audio callback
/// non-blocking under underrun.
pub struct PcmConsumer {
    inner: Consumer<i16>,
}

pub fn pcm_ring_buffer(capacity_samples: usize) -> (PcmProducer, PcmConsumer) {
    let (producer, consumer) = RingBuffer::new(capacity_samples);
    (
        PcmProducer { inner: producer },
        PcmConsumer { inner: consumer },
    )
}

impl PcmProducer {
    /// Writes as many samples as fit and drops the rest. Returns samples accepted.
    pub fn push_slice(&mut self, samples: &[i16]) -> usize {
        let mut written = 0;
        for &sample in samples {
            if self.inner.push(sample).is_err() {
                break;
            }
            written += 1;
        }
        written
    }

    pub fn free_len(&self) -> usize {
        self.inner.slots()
    }
}

impl PcmConsumer {
    /// Reads immediately, zero-filling unavailable data. Returns real samples read.
    pub fn read_into(&mut self, output: &mut [i16]) -> usize {
        let mut read = 0;
        for sample in output {
            match self.inner.pop() {
                Ok(value) => {
                    *sample = value;
                    read += 1;
                }
                Err(_) => *sample = 0,
            }
        }
        read
    }

    pub fn occupied_len(&self) -> usize {
        self.inner.slots()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_underflow_is_silence() {
        let (mut producer, mut consumer) = pcm_ring_buffer(3);
        assert_eq!(producer.push_slice(&[1, 2, 3, 4]), 3);

        let mut output = [99; 5];
        assert_eq!(consumer.read_into(&mut output), 3);
        assert_eq!(output, [1, 2, 3, 0, 0]);
    }
}
