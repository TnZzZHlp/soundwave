use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use arc_swap::ArcSwapOption;
use audio_stream_protocol::{CHANNELS, DeviceStatus, SAMPLE_RATE};
use jni::{
    EnvUnowned,
    errors::ThrowRuntimeExAndDefault,
    objects::{JClass, JShortArray, JString, ReleaseMode},
    sys::{jboolean, jint, jlong},
};
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    client,
    error::ClientError,
    ring_buffer::{PCM_RING_CAPACITY_SAMPLES, PcmRingBuffer},
    state::{ClientState, SharedState},
};

static ACTIVE_STATE: ArcSwapOption<SharedState> = ArcSwapOption::const_empty();
static ACTIVE_RING: ArcSwapOption<PcmRingBuffer> = ArcSwapOption::const_empty();
static ENGINE: OnceLock<Mutex<Option<NativeEngine>>> = OnceLock::new();

struct NativeEngine {
    runtime: Runtime,
    state: Arc<SharedState>,
    session: Option<Session>,
}

struct Session {
    task: JoinHandle<()>,
    ring: Arc<PcmRingBuffer>,
}

fn engine_slot() -> &'static Mutex<Option<NativeEngine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

fn initialize() -> Result<(), ClientError> {
    let mut slot = engine_slot()
        .lock()
        .map_err(|_| ClientError::Runtime("native engine lock was poisoned".to_owned()))?;
    if slot.is_none() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .build()
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        *slot = Some(NativeEngine {
            runtime,
            state: Arc::new(SharedState::default()),
            session: None,
        });
    }
    drop(slot);
    Ok(())
}

pub fn connect(host: &str, port: u16, fingerprint_text: &str) -> Result<(), ClientError> {
    initialize()?;
    let address: SocketAddr = format!("{host}:{port}").parse()?;
    let fingerprint = audio_stream_transport::parse_fingerprint(fingerprint_text)?;

    let mut slot = engine_slot()
        .lock()
        .map_err(|_| ClientError::Runtime("native engine lock was poisoned".to_owned()))?;
    let engine = slot
        .as_mut()
        .ok_or_else(|| ClientError::Runtime("native engine was not initialized".to_owned()))?;
    if let Some(previous) = engine.session.take() {
        previous.task.abort();
        previous.ring.clear();
    }

    let ring = Arc::new(PcmRingBuffer::new(PCM_RING_CAPACITY_SAMPLES));
    ACTIVE_RING.store(Some(ring.clone()));
    ACTIVE_STATE.store(Some(engine.state.clone()));
    engine.state.begin_connecting();
    let task = engine.runtime.spawn(client::run_session(
        address,
        fingerprint,
        ring.clone(),
        engine.state.clone(),
    ));
    engine.session = Some(Session { task, ring });
    drop(slot);
    Ok(())
}

pub fn disconnect() {
    let Ok(mut slot) = engine_slot().lock() else {
        return;
    };
    let Some(engine) = slot.as_mut() else {
        return;
    };
    if let Some(session) = engine.session.take() {
        session.task.abort();
        session.ring.clear();
    }
    ACTIVE_RING.store(None);
    engine.state.set(ClientState::Disconnected);
}

pub fn shutdown() {
    ACTIVE_RING.store(None);
    let Ok(mut slot) = engine_slot().lock() else {
        return;
    };
    let Some(mut engine) = slot.take() else {
        return;
    };
    if let Some(session) = engine.session.take() {
        session.task.abort();
        session.ring.clear();
    }
    engine.state.set(ClientState::Disconnected);
    engine.runtime.shutdown_timeout(Duration::from_millis(500));
}

pub fn read_pcm(output: &mut [i16]) -> usize {
    if ACTIVE_STATE
        .load_full()
        .is_some_and(|state| state.device.read_tone(output))
    {
        return output.len();
    }
    if let Some(ring) = ACTIVE_RING.load_full() {
        ring.read_into(output)
    } else {
        output.fill(0);
        0
    }
}

pub fn state() -> ClientState {
    engine_slot()
        .lock()
        .ok()
        .and_then(|engine| engine.as_ref().map(|engine| engine.state.state()))
        .unwrap_or(ClientState::Idle)
}

pub fn last_error() -> String {
    engine_slot()
        .lock()
        .ok()
        .and_then(|engine| engine.as_ref().map(|engine| engine.state.last_error()))
        .unwrap_or_default()
}

pub fn received_packets() -> u64 {
    with_state(super::state::SharedState::received_packets)
}

pub fn lost_packets() -> u64 {
    with_state(super::state::SharedState::lost_packets)
}

pub fn late_packets() -> u64 {
    with_state(super::state::SharedState::late_packets)
}

pub fn underruns() -> u64 {
    ACTIVE_RING.load_full().map_or(0, |ring| ring.underruns())
}

pub fn buffer_duration_ms() -> u64 {
    ACTIVE_RING.load_full().map_or(0, |ring| {
        ring.available_samples() as u64 * 1_000 / (u64::from(SAMPLE_RATE) * u64::from(CHANNELS))
    })
}

pub fn jitter_buffer_ms() -> u64 {
    if state() == ClientState::Connected {
        with_state(SharedState::jitter_buffer_ms)
    } else {
        0
    }
}

pub fn take_volume_adjustment() -> i32 {
    engine_slot()
        .lock()
        .ok()
        .and_then(|engine| {
            engine
                .as_ref()
                .map(|engine| engine.state.take_volume_adjustment())
        })
        .unwrap_or(0)
}

pub fn estimated_latency_ms() -> u64 {
    let Some((rtt_half, jitter_buffer_ms)) = engine_slot().lock().ok().and_then(|engine| {
        engine.as_ref().and_then(|engine| {
            (engine.state.state() == ClientState::Connected)
                .then(|| (engine.state.rtt_ms() / 2, engine.state.jitter_buffer_ms()))
        })
    }) else {
        return 0;
    };
    // This is deliberately an estimate, not a claim about speaker output time.
    rtt_half + buffer_duration_ms() + jitter_buffer_ms
}

/// Maps an unsigned counter to the Java `long` return type, saturating
/// instead of wrapping for the impossible overflow case.
fn to_jlong(value: u64) -> jlong {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn with_state(callback: impl FnOnce(&SharedState) -> u64) -> u64 {
    engine_slot()
        .lock()
        .ok()
        .and_then(|engine| engine.as_ref().map(|engine| callback(&engine.state)))
        .unwrap_or(0)
}

fn record_connect_error(error: &ClientError) {
    if let Ok(slot) = engine_slot().lock() {
        if let Some(engine) = slot.as_ref() {
            engine.state.set_error(error);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeConnect<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    host: JString<'local>,
    port: jint,
    fingerprint: JString<'local>,
) -> jboolean {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jboolean> {
            let host = host.mutf8_chars(env)?.to_str().into_owned();
            let fingerprint = fingerprint.mutf8_chars(env)?.to_str().into_owned();
            let result = match u16::try_from(port) {
                Ok(port) if (1..=u16::MAX).contains(&port) => connect(&host, port, &fingerprint),
                Ok(_) | Err(_) => Err(ClientError::Runtime(
                    "port must be between 1 and 65535".to_owned(),
                )),
            };
            if let Err(error) = result {
                record_connect_error(&error);
                Ok(false)
            } else {
                Ok(true)
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeDisconnect<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            disconnect();
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeShutdown<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            shutdown();
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetState<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jint> { Ok(state() as jint) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeIsAudioActive<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jboolean {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jboolean> {
            Ok(engine_slot()
                .lock()
                .ok()
                .and_then(|slot| {
                    slot.as_ref().map(|engine| {
                        engine.state.audio_active() || engine.state.device.tone_active()
                    })
                })
                .unwrap_or(false))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeWaitForUpdate<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    previous: jlong,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> {
            let state = engine_slot()
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|engine| engine.state.clone()));
            Ok(state.map_or(previous, |state| {
                to_jlong(state.wait_for_update(u64::try_from(previous).unwrap_or(u64::MAX)))
            }))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetLatencyMs<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(estimated_latency_ms())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetBufferMs<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(buffer_duration_ms())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetJitterBufferMs<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(jitter_buffer_ms())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetBufferCapacityMs<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> {
            Ok(to_jlong(
                PCM_RING_CAPACITY_SAMPLES as u64 * 1_000
                    / (u64::from(SAMPLE_RATE) * u64::from(CHANNELS)),
            ))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeTakeVolumeAdjustment<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jint> { Ok(take_volume_adjustment()) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetReceivedPackets<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(received_packets())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetLostPackets<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(lost_packets())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetLatePackets<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(late_packets())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetUnderruns<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(underruns())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetLastError<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    unowned_env
        .with_env(|env| JString::from_str(env, last_error()))
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeStopTestTone<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            if let Some(state) = ACTIVE_STATE.load_full() {
                state.device.set_tone(false);
                state.notify_changed();
            }
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetPlaybackRequest<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> {
            Ok(ACTIVE_STATE.load_full().map_or(0, |state| {
                jlong::try_from(state.device.playback_request()).unwrap_or(0)
            }))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeReportPlayback<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    request: jlong,
    playing: jboolean,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            if let Some(state) = ACTIVE_STATE.load_full() {
                state
                    .device
                    .report_playback(u64::try_from(request).unwrap_or(0), playing);
            }
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeTakeDeviceCommand<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> {
            Ok(ACTIVE_STATE.load_full().map_or(0, |state| {
                jlong::try_from(state.device.take_command()).unwrap_or(0)
            }))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeReportDevice<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    request_id: jlong,
    device_id: JString<'local>,
    name: JString<'local>,
    values: jlong,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let device_id = device_id
                .mutf8_chars(env)?
                .to_str()
                .chars()
                .take(80)
                .collect();
            let name = name.mutf8_chars(env)?.to_str().chars().take(80).collect();
            if let Some(state) = ACTIVE_STATE
                .load_full()
                .filter(|s| s.state() == ClientState::Connected)
            {
                let values = u64::try_from(values).unwrap_or(0);
                state.device.report(DeviceStatus {
                    request_id: u32::try_from(request_id).unwrap_or(0),
                    device_id,
                    name,
                    volume: u16::try_from(values & 0xffff).unwrap_or(0),
                    minimum: u16::try_from((values >> 16) & 0xffff).unwrap_or(0),
                    maximum: u16::try_from((values >> 32) & 0xffff).unwrap_or(0),
                    fixed: values & (1 << 48) != 0,
                    speaker: values & (1 << 49) != 0,
                    failed: values & (1 << 50) != 0,
                });
            }
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeReadPcm<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    output: JShortArray<'local>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
            // SAFETY: AudioService owns this ShortArray for its one playback
            // thread and never accesses it concurrently. We only fill it from
            // the lock-free PCM ring and make no JNI calls while it is borrowed.
            let mut elements = unsafe { output.get_elements(env, ReleaseMode::CopyBack)? };
            let samples = jint::try_from(read_pcm(&mut elements)).unwrap_or(jint::MAX);
            Ok(samples)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}
