/// Converts a normalized float sample to signed 16-bit PCM with clipping.
pub fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    // The cast cannot truncate: clamping keeps the scaled value within
    // [-32768.0, 32767.0] before rounding, which is exactly i16's range.
    #[allow(clippy::cast_possible_truncation)]
    if clamped >= 0.0 {
        (clamped * f32::from(i16::MAX)).round() as i16
    } else {
        (clamped * -f32::from(i16::MIN)).round() as i16
    }
}

/// Converts interleaved f32 samples into an existing i16 buffer without allocating.
pub fn interleaved_f32_to_i16(input: &[f32], output: &mut Vec<i16>) {
    output.clear();
    output.reserve(input.len().saturating_sub(output.capacity()));
    output.extend(input.iter().copied().map(f32_to_i16));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_conversion_clips_and_scales() {
        assert_eq!(f32_to_i16(-2.0), i16::MIN);
        assert_eq!(f32_to_i16(-1.0), i16::MIN);
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(2.0), i16::MAX);
    }
}
