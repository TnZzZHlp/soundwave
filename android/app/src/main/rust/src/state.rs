use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
};

const MAX_PENDING_VOLUME_STEPS: i32 = 100;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientState {
    Idle = 0,
    Connecting = 1,
    Connected = 2,
    Disconnected = 3,
    Error = 4,
}

impl ClientState {
    pub const fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Disconnected,
            4 => Self::Error,
            _ => Self::Idle,
        }
    }
}

/// Lock-free counters consulted by the UI and foreground service. The last
/// error is only accessed from UI/control calls, never from audio playback.
#[derive(Debug)]
pub struct SharedState {
    state: AtomicI32,
    audio_active: AtomicBool,
    revision: Mutex<u64>,
    changed: Condvar,
    received_packets: AtomicU64,
    lost_packets: AtomicU64,
    late_packets: AtomicU64,
    rtt_ms: AtomicU64,
    jitter_buffer_ms: AtomicU64,
    volume_adjustment: Mutex<i32>,
    last_error: Mutex<String>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            state: AtomicI32::new(ClientState::Idle as i32),
            audio_active: AtomicBool::new(false),
            revision: Mutex::new(0),
            changed: Condvar::new(),
            received_packets: AtomicU64::new(0),
            lost_packets: AtomicU64::new(0),
            late_packets: AtomicU64::new(0),
            rtt_ms: AtomicU64::new(0),
            jitter_buffer_ms: AtomicU64::new(0),
            volume_adjustment: Mutex::new(0),
            last_error: Mutex::new(String::new()),
        }
    }
}

impl SharedState {
    pub fn begin_connecting(&self) {
        self.received_packets.store(0, Ordering::Relaxed);
        self.lost_packets.store(0, Ordering::Relaxed);
        self.late_packets.store(0, Ordering::Relaxed);
        self.rtt_ms.store(0, Ordering::Relaxed);
        self.jitter_buffer_ms.store(0, Ordering::Relaxed);
        self.take_volume_adjustment();
        if let Ok(mut error) = self.last_error.lock() {
            error.clear();
        }
        self.set(ClientState::Connecting);
    }

    pub fn set(&self, state: ClientState) {
        if state != ClientState::Connected {
            self.set_audio_active(false);
        }
        if self.state.swap(state as i32, Ordering::AcqRel) != state as i32 {
            self.notify_changed();
        }
    }

    pub fn audio_active(&self) -> bool {
        self.audio_active.load(Ordering::Acquire)
    }

    pub fn set_audio_active(&self, active: bool) {
        if self.audio_active.swap(active, Ordering::AcqRel) != active {
            self.notify_changed();
        }
    }

    pub fn wait_for_update(&self, previous: u64) -> u64 {
        let revision = self
            .revision
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (revision, _) = self
            .changed
            .wait_timeout_while(revision, std::time::Duration::from_secs(30), |revision| {
                *revision == previous
            })
            .unwrap_or_else(|error| error.into_inner());
        *revision
    }

    fn notify_changed(&self) {
        let mut revision = self
            .revision
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *revision = revision.wrapping_add(1);
        self.changed.notify_all();
    }

    pub fn state(&self) -> ClientState {
        ClientState::from_raw(self.state.load(Ordering::Acquire))
    }

    pub fn set_error(&self, error: &impl ToString) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = error.to_string();
        }
        self.set(ClientState::Error);
    }

    pub fn last_error(&self) -> String {
        self.last_error.lock().map_or_else(
            |_| "native state lock was poisoned".to_owned(),
            |error| error.clone(),
        )
    }

    pub fn received_packets(&self) -> u64 {
        self.received_packets.load(Ordering::Relaxed)
    }

    pub fn lost_packets(&self) -> u64 {
        self.lost_packets.load(Ordering::Relaxed)
    }

    pub fn late_packets(&self) -> u64 {
        self.late_packets.load(Ordering::Relaxed)
    }

    pub fn rtt_ms(&self) -> u64 {
        self.rtt_ms.load(Ordering::Relaxed)
    }

    pub fn jitter_buffer_ms(&self) -> u64 {
        self.jitter_buffer_ms.load(Ordering::Relaxed)
    }

    pub fn add_received(&self) {
        self.received_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_lost(&self, count: u64) {
        self.lost_packets.fetch_add(count, Ordering::Relaxed);
    }

    pub fn add_late(&self, count: u64) {
        self.late_packets.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_rtt_ms(&self, value: u64) {
        self.rtt_ms.store(value, Ordering::Relaxed);
    }

    pub fn set_jitter_buffer_ms(&self, value: u64) {
        self.jitter_buffer_ms.store(value, Ordering::Relaxed);
    }

    pub fn add_volume_adjustment(&self, steps: i8) {
        let mut pending = self
            .volume_adjustment
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending = pending
            .saturating_add(i32::from(steps))
            .clamp(-MAX_PENDING_VOLUME_STEPS, MAX_PENDING_VOLUME_STEPS);
        drop(pending);
        self.notify_changed();
    }

    pub fn take_volume_adjustment(&self) -> i32 {
        let mut pending = self
            .volume_adjustment
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_updates_are_observable_even_before_waiting() {
        let state = SharedState::default();
        let revision = state.wait_for_update(u64::MAX);
        state.set(ClientState::Connected);
        state.set_audio_active(true);
        let connected = state.wait_for_update(revision);
        assert_ne!(connected, revision);
        assert!(state.audio_active());
        state.set(ClientState::Connecting);
        assert_ne!(state.wait_for_update(connected), connected);
        assert!(!state.audio_active());
    }
}
