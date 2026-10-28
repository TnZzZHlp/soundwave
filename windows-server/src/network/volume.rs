use std::sync::Mutex;

use tokio::sync::Notify;

#[derive(Default)]
pub struct VolumeAdjustment {
    pending: Mutex<i32>,
    changed: Notify,
}

impl VolumeAdjustment {
    pub fn add(&self, steps: i32) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending = pending.saturating_add(steps);
        self.changed.notify_one();
    }

    pub fn take(&self) -> i32 {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *pending)
    }

    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}
