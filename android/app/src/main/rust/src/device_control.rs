use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use audio_stream_protocol::{ControlMessage, DeviceStatus};
use tokio::sync::Notify;

#[derive(Debug, Default)]
pub struct DeviceControl {
    commands: Mutex<VecDeque<u64>>,
    reports: Mutex<VecDeque<DeviceStatus>>,
    tone_until: Mutex<Option<Instant>>,
    position: AtomicU64,
    speaker: AtomicBool,
    pub report_ready: Notify,
}

impl DeviceControl {
    pub fn enqueue(&self, request_id: u32, index: Option<u16>) -> bool {
        self.enqueue_operation(
            request_id,
            if index.is_some() { 2 } else { 1 },
            index.unwrap_or(0),
        )
    }

    pub fn enqueue_operation(&self, request_id: u32, operation: u8, value: u16) -> bool {
        let mut commands = self
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if commands.len() >= 32 {
            return false;
        }
        let operation = u64::from(operation);
        commands.push_back((u64::from(request_id) << 24) | (operation << 16) | u64::from(value));
        true
    }

    pub fn take_command(&self) -> u64 {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(0)
    }

    pub fn report(&self, status: DeviceStatus) {
        self.speaker
            .store(status.speaker && !status.failed, Ordering::Release);
        if !status.speaker || status.failed {
            self.set_tone(false);
        }
        let mut reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reports.len() >= 32 {
            reports.pop_front();
        }
        reports.push_back(status);
        drop(reports);
        self.report_ready.notify_one();
    }

    pub fn take_report(&self) -> Option<ControlMessage> {
        self.reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .map(ControlMessage::DeviceStatus)
    }

    pub fn set_tone(&self, enabled: bool) {
        *self
            .tone_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (enabled
            && self.speaker.load(Ordering::Acquire))
        .then(|| Instant::now() + Duration::from_secs(5));
        if !enabled {
            self.position.store(0, Ordering::Relaxed);
        }
    }

    pub fn tone_active(&self) -> bool {
        self.tone_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|until| Instant::now() < until)
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn read_tone(&self, output: &mut [i16]) -> bool {
        if !self.tone_active() {
            return false;
        }
        let frames = output.len() / 2;
        let start = self.position.fetch_add(frames as u64, Ordering::Relaxed);
        for (offset, frame) in output.chunks_exact_mut(2).enumerate() {
            let position = start.wrapping_add(offset as u64);
            let t = f64::from(u32::try_from(position % 48_000).unwrap_or(0)) / 48_000.0;
            let value: f64 = audio_stream_protocol::CALIBRATION_FREQUENCIES
                .into_iter()
                .map(|f| (std::f64::consts::TAU * f * t).sin())
                .sum();
            let fade = f64::from(u16::try_from(position.min(960)).unwrap_or(960)) / 960.0;
            frame.fill((value * fade * 600.0) as i16);
        }
        true
    }

    pub fn reset(&self) {
        self.set_tone(false);
        self.speaker.store(false, Ordering::Release);
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_session_cannot_replay_commands_or_tone() {
        let control = DeviceControl::default();
        assert!(control.enqueue(42, Some(7)));
        control.report(DeviceStatus {
            request_id: 0,
            device_id: "test".into(),
            name: "Phone".into(),
            volume: 5,
            minimum: 0,
            maximum: 15,
            fixed: false,
            speaker: true,
            failed: false,
        });
        control.set_tone(true);
        assert!(control.tone_active());
        let mut pcm = [0; 960];
        assert!(control.read_tone(&mut pcm));
        assert!(pcm.iter().all(|s| s.unsigned_abs() <= 3600));
        control.reset();
        assert_eq!(control.take_command(), 0);
        assert!(!control.tone_active());
    }
}
