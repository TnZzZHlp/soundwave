use std::sync::{
    Mutex,
    atomic::{AtomicI32, AtomicU64, Ordering},
};

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
    received_packets: AtomicU64,
    lost_packets: AtomicU64,
    late_packets: AtomicU64,
    invalid_packets: AtomicU64,
    rtt_ms: AtomicU64,
    last_error: Mutex<String>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            state: AtomicI32::new(ClientState::Idle as i32),
            received_packets: AtomicU64::new(0),
            lost_packets: AtomicU64::new(0),
            late_packets: AtomicU64::new(0),
            invalid_packets: AtomicU64::new(0),
            rtt_ms: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
        }
    }
}

impl SharedState {
    pub fn begin_connecting(&self) {
        self.received_packets.store(0, Ordering::Relaxed);
        self.lost_packets.store(0, Ordering::Relaxed);
        self.late_packets.store(0, Ordering::Relaxed);
        self.invalid_packets.store(0, Ordering::Relaxed);
        self.rtt_ms.store(0, Ordering::Relaxed);
        if let Ok(mut error) = self.last_error.lock() {
            error.clear();
        }
        self.set(ClientState::Connecting);
    }

    pub fn set(&self, state: ClientState) {
        self.state.store(state as i32, Ordering::Release);
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

    pub fn invalid_packets(&self) -> u64 {
        self.invalid_packets.load(Ordering::Relaxed)
    }

    pub fn rtt_ms(&self) -> u64 {
        self.rtt_ms.load(Ordering::Relaxed)
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

    pub fn add_invalid(&self) {
        self.invalid_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_rtt_ms(&self, value: u64) {
        self.rtt_ms.store(value, Ordering::Relaxed);
    }
}
