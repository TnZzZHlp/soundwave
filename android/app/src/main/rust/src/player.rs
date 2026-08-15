use crate::ring_buffer::PcmRingBuffer;

/// Called by the Kotlin-owned `AudioTrack` thread. Keeping this tiny boundary in
/// Rust preserves a lock-free callback path while Kotlin remains responsible
/// only for Android's platform audio API.
pub fn fill_audio_track(ring: Option<&PcmRingBuffer>, output: &mut [i16]) -> usize {
    if let Some(ring) = ring {
        ring.read_into(output)
    } else {
        output.fill(0);
        0
    }
}
