use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use wasapi::{
    Direction, SampleType, ShareMode, WasapiError, WaveFormat, get_default_device, initialize_mta,
};

use super::{PcmBlock, convert::f32_le_to_i16};

const TARGET_SAMPLE_RATE: usize = 48_000;
const TARGET_CHANNELS: usize = 2;
const TARGET_FLOAT_BYTES_PER_FRAME: usize = TARGET_CHANNELS * std::mem::size_of::<f32>();
const SILENCE_FALLBACK_BLOCKS: usize = 5;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("Windows COM initialization failed: {0}")]
    Com(String),
    #[error("WASAPI error: {0}")]
    Wasapi(#[from] WasapiError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("WASAPI returned an invalid {0}-byte f32 capture buffer")]
    InvalidCaptureBytes(usize),
    #[error("capture thread panicked")]
    ThreadPanicked,
}

/// Starts a dedicated WASAPI capture thread. The sender/receiver pair represents
/// a bounded queue; when full, the capture side evicts an old block before trying
/// to enqueue the newest block, keeping latency bounded.
pub fn spawn_loopback_capture(
    sender: Sender<PcmBlock>,
    eviction_receiver: Receiver<PcmBlock>,
    stop: CancellationToken,
    dropped_blocks: Arc<AtomicU64>,
) -> io::Result<thread::JoinHandle<Result<(), AudioError>>> {
    thread::Builder::new()
        .name("wasapi-loopback".to_owned())
        .spawn(move || {
            let result = capture_loop(sender, eviction_receiver, stop.clone(), dropped_blocks);
            if result.is_err() {
                stop.cancel();
            }
            result
        })
}

/// Debug capture mode used to validate WASAPI loopback before introducing a
/// network client. Output is raw 48 kHz stereo signed i16 little-endian PCM.
pub fn record_to_pcm(path: &Path, duration: Duration) -> Result<(), AudioError> {
    let (sender, receiver) = bounded(8);
    let stop = CancellationToken::new();
    let dropped = Arc::new(AtomicU64::new(0));
    let capture = spawn_loopback_capture(sender, receiver.clone(), stop.clone(), dropped)?;
    let mut output = File::create(path)?;
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(block) => {
                for sample in block.samples {
                    output.write_all(&sample.to_le_bytes())?;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    stop.cancel();
    capture.join().map_err(|_| AudioError::ThreadPanicked)??;
    output.flush()?;
    Ok(())
}

fn capture_loop(
    sender: Sender<PcmBlock>,
    eviction_receiver: Receiver<PcmBlock>,
    stop: CancellationToken,
    dropped_blocks: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    initialize_mta()
        .ok()
        .map_err(|error| AudioError::Com(error.to_string()))?;
    let device = get_default_device(&Direction::Render)?;
    let mut audio_client = device.get_iaudioclient()?;

    // Shared-mode WASAPI performs mature Windows sample-rate/channel conversion
    // from the actual device mix format into this fixed capture format.
    let target_format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        TARGET_SAMPLE_RATE,
        TARGET_CHANNELS,
        None,
    );
    let (default_period, _) = audio_client.get_periods()?;
    audio_client.initialize_client(
        &target_format,
        default_period,
        &Direction::Capture,
        &ShareMode::Shared,
        true,
    )?;
    let event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let max_buffer_bytes =
        audio_client.get_bufferframecount()? as usize * TARGET_FLOAT_BYTES_PER_FRAME;

    let mut raw = vec![0_u8; max_buffer_bytes.max(TARGET_FLOAT_BYTES_PER_FRAME)];
    let mut converted = Vec::with_capacity(raw.len() / std::mem::size_of::<f32>());
    let mut assembler = BlockAssembler::default();
    audio_client.start_stream()?;

    let capture_result = loop {
        if stop.is_cancelled() {
            break Ok(());
        }

        loop {
            let Some(next_frames) = capture_client.get_next_nbr_frames()? else {
                break;
            };
            if next_frames == 0 {
                break;
            }

            let byte_len = next_frames as usize * TARGET_FLOAT_BYTES_PER_FRAME;
            raw.resize(byte_len, 0);
            let (frames_read, flags) = capture_client.read_from_device(&mut raw)?;
            let sample_count = frames_read as usize * TARGET_CHANNELS;

            if flags.silent {
                assembler.push_silence(sample_count, &sender, &eviction_receiver, &dropped_blocks);
            } else {
                f32_le_to_i16(
                    &raw[..frames_read as usize * TARGET_FLOAT_BYTES_PER_FRAME],
                    &mut converted,
                )?;
                assembler.push_samples(&converted, &sender, &eviction_receiver, &dropped_blocks);
            }
        }

        match event.wait_for_event(50) {
            Ok(()) => {}
            // A device with no active render stream may not signal loopback
            // events. Emit a bounded 50 ms silent fallback so the client remains
            // clocked without racing a normally scheduled device callback.
            Err(WasapiError::EventTimeout) => assembler.push_silence(
                PcmBlock::SAMPLE_COUNT * SILENCE_FALLBACK_BLOCKS,
                &sender,
                &eviction_receiver,
                &dropped_blocks,
            ),
            Err(error) => {
                warn!(%error, "WASAPI loopback event wait failed");
                break Err(AudioError::Wasapi(error));
            }
        }
    };

    let stop_result = audio_client.stop_stream();
    capture_result?;
    stop_result?;
    debug!("WASAPI loopback capture stopped");
    Ok(())
}

struct BlockAssembler {
    timestamp: u64,
    filled: usize,
    samples: [i16; PcmBlock::SAMPLE_COUNT],
}

impl Default for BlockAssembler {
    fn default() -> Self {
        Self {
            timestamp: 0,
            filled: 0,
            samples: [0; PcmBlock::SAMPLE_COUNT],
        }
    }
}

impl BlockAssembler {
    fn push_silence(
        &mut self,
        count: usize,
        sender: &Sender<PcmBlock>,
        eviction_receiver: &Receiver<PcmBlock>,
        dropped_blocks: &AtomicU64,
    ) {
        for _ in 0..count {
            self.push_one(0, sender, eviction_receiver, dropped_blocks);
        }
    }

    fn push_samples(
        &mut self,
        samples: &[i16],
        sender: &Sender<PcmBlock>,
        eviction_receiver: &Receiver<PcmBlock>,
        dropped_blocks: &AtomicU64,
    ) {
        for &sample in samples {
            self.push_one(sample, sender, eviction_receiver, dropped_blocks);
        }
    }

    fn push_one(
        &mut self,
        sample: i16,
        sender: &Sender<PcmBlock>,
        eviction_receiver: &Receiver<PcmBlock>,
        dropped_blocks: &AtomicU64,
    ) {
        self.samples[self.filled] = sample;
        self.filled += 1;
        if self.filled != PcmBlock::SAMPLE_COUNT {
            return;
        }

        let block = PcmBlock {
            timestamp: self.timestamp,
            samples: std::mem::replace(&mut self.samples, [0; PcmBlock::SAMPLE_COUNT]),
        };
        self.timestamp = self
            .timestamp
            .wrapping_add((PcmBlock::SAMPLE_COUNT / TARGET_CHANNELS) as u64);
        self.filled = 0;
        enqueue_latest(sender, eviction_receiver, block, dropped_blocks);
    }
}

fn enqueue_latest(
    sender: &Sender<PcmBlock>,
    eviction_receiver: &Receiver<PcmBlock>,
    block: PcmBlock,
    dropped_blocks: &AtomicU64,
) -> bool {
    match sender.try_send(block) {
        Ok(()) => true,
        Err(TrySendError::Full(block)) => {
            match eviction_receiver.try_recv() {
                Ok(_) => {
                    // Evict the oldest queued block so the newly captured audio
                    // remains close to real time rather than accumulating delay.
                    dropped_blocks.fetch_add(1, Ordering::Relaxed);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    dropped_blocks.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
            match sender.try_send(block) {
                Ok(()) => true,
                Err(_) => {
                    dropped_blocks.fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            dropped_blocks.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}
