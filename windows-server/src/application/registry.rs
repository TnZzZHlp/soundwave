use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::mpsc;
use tracing::{info, warn};

use audio_stream_protocol::ControlMessage;

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
            state.receivers.insert(id, Receiver { peer, controls });
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

    /// Returns the connected receiver addresses in connect order.
    pub fn peers(&self) -> Vec<SocketAddr> {
        self.lock()
            .receivers
            .values()
            .map(|receiver| receiver.peer)
            .collect()
    }

    /// Returns how many receivers are currently connected.
    pub fn len(&self) -> usize {
        self.lock().receivers.len()
    }

    fn remove(&self, id: u64) {
        let (removed, count) = {
            let mut state = self.lock();
            let removed = state.receivers.remove(&id);
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

impl Drop for ReceiverGuard {
    fn drop(&mut self) {
        self.registry.remove(self.id);
    }
}
