use audio_stream_audio_common::{CHANNELS, SAMPLE_RATE};
use audio_stream_protocol::{SampleFormat, StreamInfo};

use crate::error::ClientError;

/// Packet sizing derived from negotiated `StreamInfo`. This keeps receiver
/// validation separate from QUIC and leaves a clear insertion point for a
/// future codec payload format.
pub struct PacketLayout {
    pub samples_per_packet: usize,
    pub samples_per_channel: u64,
    pub expected_packet_bytes: usize,
}

pub fn pcm_packet_layout(stream: StreamInfo) -> Result<PacketLayout, ClientError> {
    if stream.sample_rate != SAMPLE_RATE
        || stream.channels != CHANNELS
        || stream.sample_format != SampleFormat::I16Le
        || stream.frame_duration_ms == 0
    {
        return Err(ClientError::UnsupportedStream(format!(
            "expected 48000 Hz stereo i16, received {stream:?}"
        )));
    }

    let samples_per_channel = (SAMPLE_RATE as usize)
        .checked_mul(stream.frame_duration_ms as usize)
        .ok_or_else(|| ClientError::UnsupportedStream("frame size overflow".to_owned()))?;
    if samples_per_channel == 0 || samples_per_channel % 1_000 != 0 {
        return Err(ClientError::UnsupportedStream(
            "frame duration does not align to 48 kHz samples".to_owned(),
        ));
    }
    let samples_per_channel = samples_per_channel / 1_000;
    let samples_per_packet = samples_per_channel * CHANNELS as usize;
    let expected_packet_bytes = samples_per_packet * std::mem::size_of::<i16>();
    Ok(PacketLayout {
        samples_per_packet,
        samples_per_channel: samples_per_channel as u64,
        expected_packet_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_millisecond_pcm_layout_is_960_bytes() {
        let layout = pcm_packet_layout(StreamInfo {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::I16Le,
            frame_duration_ms: 5,
        })
        .unwrap();
        assert_eq!(layout.samples_per_channel, 240);
        assert_eq!(layout.samples_per_packet, 480);
        assert_eq!(layout.expected_packet_bytes, 960);
    }
}
