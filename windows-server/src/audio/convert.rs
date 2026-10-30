use super::capture::AudioError;

/// Converts interleaved little-endian f32 data supplied by the WASAPI shared
/// converter to i16 PCM without allocating after the output buffer is warmed up.
pub fn f32_le_to_i16(input: &[u8], output: &mut Vec<i16>) -> Result<(), AudioError> {
    let (samples, remainder) = input.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(AudioError::InvalidCaptureBytes(input.len()));
    }

    output.clear();
    output.reserve(input.len() / std::mem::size_of::<f32>());
    for &bytes in samples {
        let sample = f32::from_le_bytes(bytes);
        let sample = sample.clamp(-1.0, 1.0);
        #[allow(clippy::cast_possible_truncation)]
        output.push(if sample >= 0.0 {
            (sample * f32::from(i16::MAX)).round() as i16
        } else {
            (sample * -f32::from(i16::MIN)).round() as i16
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_little_endian_float_pcm() {
        let input = [0.0_f32, -1.0, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        f32_le_to_i16(&input, &mut output).unwrap();
        assert_eq!(output, [0, i16::MIN, i16::MAX]);
    }
}
