use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::mpsc;
use tracing::{info, warn};

use audio_stream_protocol::{CalibrationAction, ControlMessage, DeviceStatus, VolumePurpose};

static NEXT_REQUEST: AtomicU32 = AtomicU32::new(1);

#[derive(Clone)]
pub struct ReceiverSnapshot {
    pub id: u64,
    pub peer: SocketAddr,
    pub device: Option<DeviceStatus>,
    pub playing: bool,
}

/// Receiver count above which consumer Wi-Fi airtime is likely to degrade.
///
/// This is a diagnostic threshold, not a connection limit: every receiver has
/// independent congestion control, so the server never rejects a peer because
/// of it.
pub const AIRTIME_WARNING_RECEIVERS: usize = 4;

/// Tracks connected receivers and fans server-side controls out to all of them.
#[derive(Clone, Default)]
pub struct ReceiverRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    next_id: u64,
    receivers: BTreeMap<u64, Receiver>,
    airtime_warned: bool,
}

struct Receiver {
    peer: SocketAddr,
    controls: mpsc::Sender<ControlMessage>,
    device: Option<DeviceStatus>,
    replies: BTreeMap<u32, DeviceStatus>,
    tone_replies: BTreeMap<u32, (bool, bool)>,
    playback: Arc<AtomicBool>,
    unsolicited_volume: Option<u16>,
    observed_volume: Option<u16>,
    volume_requests: BTreeSet<u32>,
    panel_barrier: Option<u32>,
}

/// A registered receiver; dropping it removes the receiver from the registry.
pub struct ReceiverGuard {
    registry: ReceiverRegistry,
    id: u64,
}

impl ReceiverRegistry {
    /// Publishes a freshly ready receiver and returns the send side of its
    /// control-message queue. Dropping the returned guard deregisters it.
    pub fn register(
        &self,
        peer: SocketAddr,
        controls: mpsc::Sender<ControlMessage>,
    ) -> ReceiverGuard {
        let (id, count) = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            state.receivers.insert(
                id,
                Receiver {
                    peer,
                    controls,
                    device: None,
                    replies: BTreeMap::new(),
                    tone_replies: BTreeMap::new(),
                    playback: Arc::new(AtomicBool::new(false)),
                    unsolicited_volume: None,
                    observed_volume: None,
                    volume_requests: BTreeSet::new(),
                    panel_barrier: None,
                },
            );
            (id, state.receivers.len())
        };

        info!(%peer, receivers = count, "receiver joined");
        let warn_airtime = {
            let mut state = self.lock();
            if count > AIRTIME_WARNING_RECEIVERS && !state.airtime_warned {
                state.airtime_warned = true;
                true
            } else {
                false
            }
        };
        if warn_airtime {
            warn!(
                receivers = count,
                "streaming to more than {AIRTIME_WARNING_RECEIVERS} receivers; \
                 every receiver costs another copy of the same PCM stream over Wi-Fi"
            );
        }

        ReceiverGuard {
            registry: self.clone(),
            id,
        }
    }

    /// Sends one control message to every connected receiver. Receivers whose
    /// queue is gone are skipped so one dropped peer cannot stall the rest.
    pub fn broadcast(&self, message: ControlMessage) {
        let targets: Vec<mpsc::Sender<ControlMessage>> = {
            let state = self.lock();
            state
                .receivers
                .values()
                .map(|receiver| receiver.controls.clone())
                .collect()
        };

        for controls in targets {
            if let Err(error) = controls.try_send(message.clone()) {
                warn!(%error, "could not queue a control message for a receiver");
            }
        }
    }

    pub fn snapshots(&self) -> Vec<ReceiverSnapshot> {
        self.lock()
            .receivers
            .iter()
            .map(|(&id, r)| ReceiverSnapshot {
                id,
                peer: r.peer,
                device: r.device.clone(),
                playing: r.playback.load(Ordering::Acquire),
            })
            .collect()
    }

    pub fn playback_gate(&self, id: u64) -> Arc<AtomicBool> {
        self.lock()
            .receivers
            .get(&id)
            .map(|r| r.playback.clone())
            .unwrap_or_default()
    }

    pub fn deny_all(&self) {
        for receiver in self.lock().receivers.values() {
            receiver.playback.store(false, Ordering::Release);
        }
    }

    pub fn approve(&self, expected: &[(u64, DeviceStatus)]) {
        let state = self.lock();
        let ready = expected.iter().all(|(id, expected)| {
            state
                .receivers
                .get(id)
                .and_then(|r| r.device.as_ref())
                .is_some_and(|actual| {
                    actual.device_id == expected.device_id
                        && actual.minimum == expected.minimum
                        && actual.maximum == expected.maximum
                        && actual.volume == expected.volume
                        && actual.speaker
                        && !actual.fixed
                        && !actual.failed
                })
        });
        for (id, receiver) in &state.receivers {
            receiver.playback.store(
                ready && expected.iter().any(|(approved, _)| approved == id),
                Ordering::Release,
            );
        }
    }

    pub fn send(&self, id: u64, message: ControlMessage) -> anyhow::Result<()> {
        let controls = self
            .lock()
            .receivers
            .get(&id)
            .map(|r| r.controls.clone())
            .ok_or_else(|| anyhow::anyhow!("设备已断开连接"))?;
        controls
            .try_send(message)
            .map_err(|e| anyhow::anyhow!("无法发送设备控制命令：{e}"))
    }

    pub fn request(&self, id: u64, index: Option<u16>) -> anyhow::Result<u32> {
        if let Some(index) = index {
            return self.volume_request(id, index, VolumePurpose::User);
        }
        let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed).max(1);
        self.send(id, ControlMessage::QueryDevice { request_id })?;
        Ok(request_id)
    }

    pub fn calibration_volume_request(&self, id: u64, index: u16) -> anyhow::Result<u32> {
        self.volume_request(id, index, VolumePurpose::Calibration)
    }

    pub fn restore_volume_request(&self, id: u64, index: u16) -> anyhow::Result<u32> {
        self.volume_request(id, index, VolumePurpose::RestoreSpeaker)
    }

    pub fn panel_volume_request(&self, id: u64, index: u16) -> anyhow::Result<u32> {
        self.volume_request_with_barrier(id, index, VolumePurpose::RestoreSpeaker, true)
    }

    fn volume_request(&self, id: u64, index: u16, purpose: VolumePurpose) -> anyhow::Result<u32> {
        self.volume_request_with_barrier(id, index, purpose, false)
    }

    fn volume_request_with_barrier(
        &self,
        id: u64,
        index: u16,
        purpose: VolumePurpose,
        barrier: bool,
    ) -> anyhow::Result<u32> {
        let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed).max(1);
        let mut state = self.lock();
        let receiver = state
            .receivers
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("设备已断开连接"))?;
        receiver
            .controls
            .try_send(ControlMessage::SetVolume {
                request_id,
                index,
                purpose,
            })
            .map_err(|e| anyhow::anyhow!("无法发送设备控制命令：{e}"))?;
        if receiver.volume_requests.len() >= 32 {
            receiver.volume_requests.pop_first();
        }
        receiver.volume_requests.insert(request_id);
        if barrier {
            receiver.panel_barrier = Some(request_id);
            receiver.unsolicited_volume = None;
        }
        Ok(request_id)
    }

    pub fn calibration_request(&self, id: u64, action: CalibrationAction) -> anyhow::Result<u32> {
        let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed).max(1);
        self.send(
            id,
            ControlMessage::CalibrationSession { request_id, action },
        )?;
        Ok(request_id)
    }

    pub fn tone_request(&self, id: u64, enabled: bool) -> anyhow::Result<u32> {
        let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed).max(1);
        self.send(
            id,
            ControlMessage::CalibrationTone {
                request_id,
                enabled,
            },
        )?;
        Ok(request_id)
    }

    pub fn update_tone(&self, id: u64, request_id: u32, playing: bool, failed: bool) {
        if request_id == 0 {
            return;
        }
        let mut state = self.lock();
        if let Some(receiver) = state.receivers.get_mut(&id) {
            if receiver.tone_replies.len() >= 32 {
                receiver.tone_replies.pop_first();
            }
            receiver.tone_replies.insert(request_id, (playing, failed));
        }
    }

    pub fn take_tone_reply(&self, id: u64, request_id: u32) -> Option<(bool, bool)> {
        self.lock()
            .receivers
            .get_mut(&id)?
            .tone_replies
            .remove(&request_id)
    }

    pub fn take_reply(&self, id: u64, request_id: u32) -> Option<DeviceStatus> {
        self.lock()
            .receivers
            .get_mut(&id)?
            .replies
            .remove(&request_id)
    }

    pub fn has_panel_barrier(&self, id: u64) -> bool {
        self.lock()
            .receivers
            .get(&id)
            .is_some_and(|receiver| receiver.panel_barrier.is_some())
    }

    pub fn take_unsolicited_volume(&self, id: u64) -> Option<u16> {
        self.lock()
            .receivers
            .get_mut(&id)?
            .unsolicited_volume
            .take()
    }

    pub fn update_device(&self, id: u64, mut status: DeviceStatus) {
        if status.minimum > status.maximum
            || status.volume > status.maximum
            || status.volume < status.minimum
            || status.maximum > 512
        {
            return;
        }
        status.name = status
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect();
        status.device_id = status
            .device_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(80)
            .collect();
        let mut state = self.lock();
        if let Some(receiver) = state.receivers.get_mut(&id) {
            let same_device = receiver.device.as_ref().is_some_and(|old| {
                old.device_id == status.device_id
                    && old.minimum == status.minimum
                    && old.maximum == status.maximum
            });
            if !same_device {
                receiver.observed_volume = Some(status.volume);
                receiver.unsolicited_volume = None;
            } else if status.request_id == 0 && !status.failed && !status.fixed && status.speaker {
                if receiver.panel_barrier.is_none()
                    && receiver
                        .observed_volume
                        .is_some_and(|volume| volume != status.volume)
                {
                    receiver.unsolicited_volume = Some(status.volume);
                }
                receiver.observed_volume = Some(status.volume);
            } else if receiver.volume_requests.remove(&status.request_id)
                && (!status.failed || (!status.fixed && status.speaker))
            {
                receiver.observed_volume = Some(status.volume);
            }
            if receiver.panel_barrier == Some(status.request_id) {
                receiver.panel_barrier = None;
            }
            if receiver.device.as_ref().is_none_or(|old| {
                old.volume != status.volume
                    || old.device_id != status.device_id
                    || old.minimum != status.minimum
                    || old.maximum != status.maximum
                    || old.speaker != status.speaker
                    || old.failed != status.failed
                    || old.fixed != status.fixed
            }) {
                receiver.playback.store(false, Ordering::Release);
            }
            receiver.device = Some(status.clone());
            if status.request_id != 0 {
                if receiver.replies.len() >= 32
                    && let Some(oldest) = receiver.replies.keys().next().copied()
                {
                    receiver.replies.remove(&oldest);
                }
                receiver.replies.insert(status.request_id, status);
            }
        }
    }

    fn remove(&self, id: u64) {
        let (removed, count) = {
            let mut state = self.lock();
            let removed = state.receivers.remove(&id);
            if removed.is_some() {
                for receiver in state.receivers.values() {
                    receiver.playback.store(false, Ordering::Release);
                }
            }
            if state.receivers.len() <= AIRTIME_WARNING_RECEIVERS {
                state.airtime_warned = false;
            }
            (removed, state.receivers.len())
        };
        if let Some(receiver) = removed {
            info!(peer = %receiver.peer, receivers = count, "receiver left");
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl ReceiverGuard {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for ReceiverGuard {
    fn drop(&mut self) {
        self.registry.remove(self.id);
    }
}
