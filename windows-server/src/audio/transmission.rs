use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct AudioTransmissionGate {
    inner: Arc<GateState>,
}

struct GateState {
    enabled: AtomicBool,
    calibrating: AtomicBool,
    mutex: Mutex<()>,
    changed: Condvar,
}

impl AudioTransmissionGate {
    pub fn enabled() -> Self {
        Self {
            inner: Arc::new(GateState {
                enabled: AtomicBool::new(true),
                calibrating: AtomicBool::new(false),
                mutex: Mutex::new(()),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
            && !self.inner.calibrating.load(Ordering::Acquire)
    }

    pub fn set_calibrating(&self, calibrating: bool) {
        let _guard = self.inner.mutex.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.calibrating.store(calibrating, Ordering::Release);
        self.inner.changed.notify_all();
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _guard = self
            .inner
            .mutex
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.inner.enabled.store(enabled, Ordering::Release);
        self.inner.changed.notify_all();
    }

    pub fn wait_until_enabled(&self, stop: &CancellationToken) {
        let mut guard = self
            .inner
            .mutex
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while !self.is_enabled() && !stop.is_cancelled() {
            (guard, _) = self
                .inner
                .changed
                .wait_timeout(guard, Duration::from_millis(500))
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}
