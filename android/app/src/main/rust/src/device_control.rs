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

#[derive(Debug)]
struct PendingCommand {
    value: u64,
    expires: Option<Instant>,
}

#[derive(Debug, Default)]
struct Tone {
    token: u64,
    until: Option<Instant>,
    responded: bool,
}

#[derive(Debug, Default)]
pub struct DeviceControl {
    commands: Mutex<VecDeque<PendingCommand>>,
    reports: Mutex<VecDeque<ControlMessage>>,
    tone: Mutex<Tone>,
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
        commands.push_back(PendingCommand {
            value: (u64::from(request_id) << 24) | (u64::from(operation) << 16) | u64::from(value),
            expires: (operation != 4).then(|| Instant::now() + Duration::from_secs(2)),
        });
        true
    }

    pub fn take_command(&self) -> u64 {
        self.take_command_at(Instant::now())
    }

    fn take_command_at(&self, now: Instant) -> u64 {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .map_or(0, |command| {
                if command.expires.is_some_and(|expires| now >= expires) {
                    (command.value & (!0_u64 << 24)) | (8 << 16)
                } else {
                    command.value
                }
            })
    }

    fn push_report(&self, report: ControlMessage) {
        let mut reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reports.len() >= 32 {
            reports.pop_front();
        }
        reports.push_back(report);
        drop(reports);
        self.report_ready.notify_one();
    }

    pub fn report(&self, status: DeviceStatus) {
        self.speaker
            .store(status.speaker && !status.failed, Ordering::Release);
        if !status.speaker || status.failed {
            self.set_tone(false);
        }
        self.push_report(ControlMessage::DeviceStatus(status));
    }

    pub fn take_report(&self) -> Option<ControlMessage> {
        self.reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    pub fn request_tone(&self, request_id: u32, enabled: bool) {
        let allowed = !enabled || self.speaker.load(Ordering::Acquire);
        {
            let mut tone = self
                .tone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tone.token = (u64::from(request_id) << 1) | u64::from(enabled);
            tone.until = (enabled && allowed).then(|| Instant::now() + Duration::from_secs(5));
            tone.responded = request_id == 0 || !allowed;
        }
        self.position.store(0, Ordering::Relaxed);
        if !allowed && request_id != 0 {
            self.push_report(ControlMessage::ToneState {
                request_id,
                playing: false,
                failed: true,
            });
        }
    }

    pub fn set_tone(&self, enabled: bool) {
        if enabled {
            self.request_tone(0, true);
            return;
        }
        let failed = {
            let mut tone = self
                .tone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_active = tone.until.take().is_some();
            let request_id = (tone.token >> 1) as u32;
            tone.token &= !1;
            tone.responded |= was_active;
            (was_active && request_id != 0).then_some(request_id)
        };
        if let Some(request_id) = failed {
            self.push_report(ControlMessage::ToneState {
                request_id,
                playing: false,
                failed: true,
            });
        }
        self.position.store(0, Ordering::Relaxed);
    }

    fn expire_tone(&self, now: Instant) {
        let failed = {
            let mut tone = self
                .tone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if tone.until.is_some_and(|until| now >= until) {
                tone.until = None;
                let request_id = (tone.token >> 1) as u32;
                tone.token &= !1;
                tone.responded = true;
                (request_id != 0).then_some(request_id)
            } else {
                None
            }
        };
        if let Some(request_id) = failed {
            self.push_report(ControlMessage::ToneState {
                request_id,
                playing: false,
                failed: true,
            });
        }
    }

    pub fn playback_request(&self) -> u64 {
        self.expire_tone(Instant::now());
        self.tone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .token
    }

    pub fn report_playback(&self, token: u64, playing: bool) {
        self.expire_tone(Instant::now());
        if token >> 1 == 0 {
            return;
        }
        let report = {
            let mut tone = self
                .tone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if token != tone.token {
                return;
            }
            let enabled = token & 1 != 0;
            let active = tone.until.is_some_and(|until| Instant::now() < until);
            if enabled && playing && active && !tone.responded {
                tone.until = Some(Instant::now() + Duration::from_secs(5));
                tone.responded = true;
                Some((true, false))
            } else if !enabled && !playing && !tone.responded {
                tone.responded = true;
                Some((false, false))
            } else if enabled
                && !playing
                && !active
                && (tone.until.take().is_some() || !tone.responded)
            {
                tone.token &= !1;
                tone.responded = true;
                Some((false, true))
            } else {
                None
            }
        };
        if let Some((playing, failed)) = report {
            self.push_report(ControlMessage::ToneState {
                request_id: (token >> 1) as u32,
                playing,
                failed,
            });
        }
    }

    pub fn tone_active(&self) -> bool {
        self.expire_tone(Instant::now());
        self.tone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .until
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
        *self
            .tone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Tone::default();
        self.position.store(0, Ordering::Relaxed);
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

    #[test]
    fn playback_acknowledgement_requires_the_current_audio_worker_token() {
        let control = DeviceControl::default();
        control.speaker.store(true, Ordering::Release);
        control.request_tone(7, true);
        assert!(control.take_report().is_none());
        control.report_playback(13, true);
        assert!(control.take_report().is_none());
        control.report_playback(15, true);
        assert_eq!(
            control.take_report(),
            Some(ControlMessage::ToneState {
                request_id: 7,
                playing: true,
                failed: false
            })
        );
        control.request_tone(8, false);
        control.report_playback(15, false);
        assert!(control.take_report().is_none());
        control.report_playback(16, false);
        assert_eq!(
            control.take_report(),
            Some(ControlMessage::ToneState {
                request_id: 8,
                playing: false,
                failed: false
            })
        );
    }

    #[test]
    fn forced_stop_invalidates_pcm_already_read_by_the_worker() {
        let control = DeviceControl::default();
        control.speaker.store(true, Ordering::Release);
        control.request_tone(7, true);
        let read_token = control.playback_request();
        assert!(control.read_tone(&mut [0; 480]));
        control.set_tone(false);
        assert_ne!(control.playback_request(), read_token);
        assert_eq!(
            control.take_report(),
            Some(ControlMessage::ToneState {
                request_id: 7,
                playing: false,
                failed: true
            })
        );
        control.report_playback(read_token, true);
        assert!(control.take_report().is_none());
    }

    #[test]
    fn expiry_invalidates_the_token_without_waiting_for_pcm_silence() {
        let control = DeviceControl::default();
        control.speaker.store(true, Ordering::Release);
        control.request_tone(7, true);
        let token = control.playback_request();
        control.expire_tone(Instant::now() + Duration::from_secs(6));
        assert_ne!(control.playback_request(), token);
        assert!(!control.tone_active());
        assert_eq!(
            control.take_report(),
            Some(ControlMessage::ToneState {
                request_id: 7,
                playing: false,
                failed: true
            })
        );
    }

    #[test]
    fn stale_commands_are_rejected_but_cleanup_still_runs() {
        let control = DeviceControl::default();
        assert!(control.enqueue_operation(42, 6, 10));
        assert!(control.enqueue_operation(43, 4, 0));
        let delayed = Instant::now() + Duration::from_secs(3);
        assert_eq!(control.take_command_at(delayed), (42 << 24) | (8 << 16));
        assert_eq!(control.take_command_at(delayed), (43 << 24) | (4 << 16));
    }
}
