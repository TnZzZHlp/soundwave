/// Converts a normalized float sample to signed 16-bit PCM with clipping.
pub fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped >= 0.0 {
        (clamped * i16::MAX as f32).round() as i16
    } else {
        (clamped * -(i16::MIN as f32)).round() as i16
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
