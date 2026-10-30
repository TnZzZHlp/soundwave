use std::{net::SocketAddr, path::PathBuf};

use tokio_util::sync::CancellationToken;

use super::{calibration::CalibrationController, registry::ReceiverRegistry};
use crate::{audio::transmission::AudioTransmissionGate, pairing::PairingInfo};

pub(crate) struct ServerInfo {
    pub device_name: String,
    pub mix_sample_rate: u32,
    pub mix_channels: u16,
    pub mix_bits_per_sample: u16,
    pub mix_sample_type: String,
    pub bind: SocketAddr,
    pub fingerprint: String,
    pub identity_dir: PathBuf,
    pub pairing: Result<PairingInfo, String>,
}

pub(crate) struct ServerState {
    pub info: ServerInfo,
    pub shutdown: CancellationToken,
    pub transmission: AudioTransmissionGate,
    pub registry: ReceiverRegistry,
    pub calibration: CalibrationController,
}

impl ServerState {
    pub fn new(
        info: ServerInfo,
        shutdown: CancellationToken,
        transmission: AudioTransmissionGate,
        registry: ReceiverRegistry,
    ) -> Self {
        let calibration =
            CalibrationController::load(&info.identity_dir.join("volume-calibration.txt"));
        Self {
            info,
            shutdown,
            transmission,
            registry,
            calibration,
        }
    }
}
