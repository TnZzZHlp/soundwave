use wasapi::{Direction, get_default_device, initialize_mta};

use super::capture::AudioError;

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub friendly_name: String,
    pub mix_sample_rate: u32,
    pub mix_channels: u16,
    pub mix_bits_per_sample: u16,
    pub mix_sample_type: String,
}

pub fn default_render_device_info() -> Result<DeviceInfo, AudioError> {
    initialize_mta()
        .ok()
        .map_err(|error| AudioError::Com(error.to_string()))?;
    let device = get_default_device(&Direction::Render)?;
    let friendly_name = device.get_friendlyname()?;
    let client = device.get_iaudioclient()?;
    let mix = client.get_mixformat()?;

    Ok(DeviceInfo {
        friendly_name,
        mix_sample_rate: mix.get_samplespersec(),
        mix_channels: mix.get_nchannels(),
        mix_bits_per_sample: mix.get_bitspersample(),
        mix_sample_type: format!("{:?}", mix.get_subformat()?),
    })
}
