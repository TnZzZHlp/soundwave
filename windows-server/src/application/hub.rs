use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Context;
use crossbeam_channel::bounded;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    audio::{PcmBlock, capture::spawn_loopback_capture, transmission::AudioTransmissionGate},
    network::server::FRAME_QUEUE_CAPACITY,
};

/// Blocks buffered for receivers between capture and their send tasks.
///
/// Two blocks are 20 ms: enough that a receiver waking a few milliseconds late
/// still sees fresh audio, while an older block is dropped rather than queued.
const CAPTURE_BROADCAST_CAPACITY: usize = 2;

/// One shared WASAPI capture feeding every connected receiver.
///
/// Capture is reference counted: the first subscriber starts the WASAPI thread
/// and the last unsubscribe stops and joins it. All receivers therefore share a
/// single sample timeline, which keeps sequence numbers and timestamps
/// comparable across peers and avoids running one capture thread per phone.
#[derive(Clone)]
pub struct CaptureHub {
    inner: Arc<HubInner>,
}

/// Starts one capture run and returns its handle. Production code uses WASAPI;
/// tests substitute a fake source to exercise the refcount without a device.
type CaptureStarter = dyn Fn() -> anyhow::Result<CaptureHandle> + Send + Sync;

struct HubInner {
    state: Mutex<HubState>,
    dropped_blocks: Arc<AtomicU64>,
    start_capture: Box<CaptureStarter>,
}

#[derive(Default)]
struct HubState {
    capture: Option<CaptureHandle>,
    subscribers: usize,
}

pub struct CaptureHandle {
    stop: CancellationToken,
    thread: JoinHandle<Result<(), crate::audio::capture::AudioError>>,
    blocks: broadcast::Sender<Arc<PcmBlock>>,
}

impl CaptureHandle {
    /// Creates a handle from a capture thread and its broadcast sender.
    fn new(
        stop: CancellationToken,
        thread: JoinHandle<Result<(), crate::audio::capture::AudioError>>,
        blocks: broadcast::Sender<Arc<PcmBlock>>,
    ) -> Self {
        Self {
            stop,
            thread,
            blocks,
        }
    }
}

impl CaptureHub {
    /// Creates a hub that starts WASAPI capture on the first subscription.
    pub fn new(transmission: AudioTransmissionGate) -> Self {
        let dropped_blocks = Arc::new(AtomicU64::new(0));
        let starter_dropped = dropped_blocks.clone();
        let starter_transmission = transmission.clone();
        Self::with_starter(dropped_blocks, move || {
            start_wasapi_capture(starter_dropped.clone(), starter_transmission.clone())
        })
    }

    fn with_starter(
        dropped_blocks: Arc<AtomicU64>,
        start_capture: impl Fn() -> anyhow::Result<CaptureHandle> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(HubInner {
                state: Mutex::new(HubState::default()),
                dropped_blocks,
                start_capture: Box::new(start_capture),
            }),
        }
    }

    /// Subscribes a receiver, starting the shared capture on the first
    /// subscription. The returned receiver yields every captured block; a slow
    /// consumer reports `Lagged` instead of blocking capture.
    pub fn subscribe(&self) -> anyhow::Result<broadcast::Receiver<Arc<PcmBlock>>> {
        let mut state = self.lock();
        if state.capture.is_none() {
            state.capture = Some((self.inner.start_capture)()?);
            info!("started shared WASAPI capture for the first receiver");
        }
        state.subscribers += 1;
        let blocks = state
            .capture
            .as_ref()
            .context("shared capture was not started")?
            .blocks
            .subscribe();
        Ok(blocks)
    }

    /// Drops one subscription, stopping and joining capture when the last
    /// receiver leaves.
    pub async fn unsubscribe(&self) {
        let capture = {
            let mut state = self.lock();
            state.subscribers = state.subscribers.saturating_sub(1);
            if state.subscribers > 0 {
                None
            } else {
                state.capture.take()
            }
        };
        let Some(capture) = capture else {
            return;
        };

        capture.stop.cancel();
        // Joining the WASAPI thread blocks until its 50 ms event wait returns,
        // so keep it off the async runtime workers.
        let joined = tokio::task::spawn_blocking(move || Self::join_capture(capture)).await;
        match joined {
            Ok(Err(error)) => warn!(%error, "shared WASAPI capture ended with an error"),
            Err(error) => warn!(%error, "shared capture join task panicked"),
            Ok(Ok(())) => info!("stopped shared WASAPI capture after the last receiver left"),
        }
    }

    /// Reports capture queue drops observed while feeding subscribers.
    pub fn dropped_blocks(&self) -> u64 {
        self.inner.dropped_blocks.load(Ordering::Relaxed)
    }

    fn join_capture(capture: CaptureHandle) -> anyhow::Result<()> {
        capture
            .thread
            .join()
            .map_err(|_| anyhow::anyhow!("WASAPI capture thread panicked"))?
            .map_err(|error| anyhow::anyhow!(error))
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl Drop for HubInner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(capture) = state.capture.take() {
            capture.stop.cancel();
            let _ = CaptureHub::join_capture(capture);
        }
    }
}

/// Starts the real WASAPI capture thread plus the thread that republishes its
/// 10 ms blocks to every subscribed receiver.
fn start_wasapi_capture(
    dropped_blocks: Arc<AtomicU64>,
    transmission: AudioTransmissionGate,
) -> anyhow::Result<CaptureHandle> {
    let (blocks, _) = broadcast::channel(CAPTURE_BROADCAST_CAPACITY);
    let (sender, receiver) = bounded(FRAME_QUEUE_CAPACITY);
    let stop = CancellationToken::new();
    let thread = spawn_loopback_capture(
        sender,
        receiver.clone(),
        stop.clone(),
        dropped_blocks,
        transmission,
    )
    .context("could not start WASAPI loopback thread")?;

    let publisher_stop = stop.clone();
    let publish = blocks.clone();
    thread::Builder::new()
        .name("capture-fanout".to_owned())
        .spawn(move || {
            while !publisher_stop.is_cancelled() {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(block) => {
                        // A send error only means no receiver is listening right
                        // now; capture keeps running for the session.
                        let _ = publish.send(Arc::new(block));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
            debug!("capture fan-out thread stopped");
        })
        .context("could not start capture fan-out thread")?;

    Ok(CaptureHandle::new(stop, thread, blocks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A capture handle whose "capture thread" only records that it was
    /// stopped, so refcount behaviour is testable without a WASAPI device.
    fn fake_capture(
        starts: Arc<AtomicUsize>,
        blocks: broadcast::Sender<Arc<PcmBlock>>,
    ) -> CaptureHandle {
        starts.fetch_add(1, Ordering::Relaxed);
        let stop = CancellationToken::new();
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        });
        CaptureHandle::new(stop, thread, blocks)
    }

    #[test]
    fn reconnecting_receiver_keeps_capture_running_for_the_other_receiver() {
        let starts = Arc::new(AtomicUsize::new(0));
        let starter_count = starts.clone();
        let hub = CaptureHub::with_starter(Arc::new(AtomicU64::new(0)), move || {
            let (blocks, _) = broadcast::channel(2);
            Ok(fake_capture(starter_count.clone(), blocks))
        });
        let mut first = hub.subscribe().unwrap();
        let second_hub = hub.clone();
        let second = second_hub.subscribe().unwrap();
        let (stop, blocks) = {
            let state = hub.lock();
            let capture = state.capture.as_ref().unwrap();
            (capture.stop.clone(), capture.blocks.clone())
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        drop(second);
        runtime.block_on(second_hub.unsubscribe());
        drop(second_hub);
        assert!(!stop.is_cancelled());

        let reconnected_hub = hub.clone();
        let mut reconnected = reconnected_hub.subscribe().unwrap();
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        blocks
            .send(Arc::new(PcmBlock {
                timestamp: 42,
                samples: [1; PcmBlock::SAMPLE_COUNT],
            }))
            .unwrap();
        assert_eq!(first.try_recv().unwrap().timestamp, 42);
        assert_eq!(reconnected.try_recv().unwrap().timestamp, 42);

        drop(reconnected);
        runtime.block_on(reconnected_hub.unsubscribe());
        drop(reconnected_hub);
        assert!(!stop.is_cancelled());
        drop(first);
        runtime.block_on(hub.unsubscribe());
        assert!(stop.is_cancelled());
    }

    #[test]
    fn capture_starts_once_for_overlapping_receivers_and_stops_after_the_last() {
        let starts = Arc::new(AtomicUsize::new(0));
        let starter_count = starts.clone();
        let hub = CaptureHub::with_starter(Arc::new(AtomicU64::new(0)), move || {
            let (blocks, _) = broadcast::channel(2);
            Ok(fake_capture(starter_count.clone(), blocks))
        });

        let first = hub.subscribe().unwrap();
        let second = hub.subscribe().unwrap();
        assert_eq!(
            starts.load(Ordering::Relaxed),
            1,
            "a second receiver must not start another capture"
        );
        drop(first);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(hub.unsubscribe());
        assert_eq!(
            starts.load(Ordering::Relaxed),
            1,
            "capture keeps running while a receiver remains"
        );

        drop(second);
        runtime.block_on(hub.unsubscribe());
        assert!(
            hub.lock().capture.is_none(),
            "capture stopped after the last receiver"
        );
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::*;

    fn block(timestamp: u64) -> PcmBlock {
        PcmBlock {
            timestamp,
            samples: [0; PcmBlock::SAMPLE_COUNT],
        }
    }

    #[test]
    fn lagging_receiver_skips_old_blocks_without_slowing_the_others() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (blocks, _) = broadcast::channel(CAPTURE_BROADCAST_CAPACITY);
            let mut fast = blocks.subscribe();
            let mut slow = blocks.subscribe();

            for timestamp in 0..6 {
                blocks.send(Arc::new(block(timestamp))).unwrap();
                // The healthy receiver keeps pace with capture.
                assert_eq!(fast.recv().await.unwrap().timestamp, timestamp);
            }

            // The stalled receiver never blocked capture: it reports the blocks
            // it missed and resumes at the newest one.
            let pending = tokio::time::timeout(Duration::from_millis(10), fast.recv()).await;
            assert!(
                pending.is_err(),
                "no extra block is pending for the fast receiver"
            );

            // The stalled receiver reports the blocks it fell behind on and
            // then resumes from the oldest block still buffered, so its
            // staleness stays bounded by the channel capacity.
            match slow.recv().await {
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    assert_eq!(skipped, 4, "skips exactly the blocks it fell behind on");
                }
                other => panic!("expected a lag report, got {other:?}"),
            }
            assert_eq!(slow.recv().await.unwrap().timestamp, 4);
            assert_eq!(slow.recv().await.unwrap().timestamp, 5);
        });
    }
}
