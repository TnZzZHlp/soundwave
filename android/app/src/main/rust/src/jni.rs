use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use arc_swap::ArcSwapOption;
use audio_stream_audio_common::{CHANNELS, SAMPLE_RATE};
use jni::{
    EnvUnowned,
    errors::ThrowRuntimeExAndDefault,
    objects::{JClass, JShortArray, JString, ReleaseMode},
    sys::{jboolean, jint, jlong},
};
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    client,
    error::ClientError,
    player::fill_audio_track,
    ring_buffer::{PCM_RING_CAPACITY_SAMPLES, PcmRingBuffer},
    state::{ClientState, SharedState},
};

static ACTIVE_RING: ArcSwapOption<PcmRingBuffer> = ArcSwapOption::const_empty();
static ENGINE: OnceLock<Mutex<Option<NativeEngine>>> = OnceLock::new();

struct NativeEngine {
    runtime: Runtime,
    state: Arc<SharedState>,
    session: Option<Session>,
}

struct Session {
    stop: CancellationToken,
    task: JoinHandle<()>,
    ring: Arc<PcmRingBuffer>,
}

fn engine_slot() -> &'static Mutex<Option<NativeEngine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub fn initialize() -> Result<(), ClientError> {
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
        previous.stop.cancel();
        previous.task.abort();
        previous.ring.clear();
    }

    let ring = Arc::new(PcmRingBuffer::new(PCM_RING_CAPACITY_SAMPLES));
    ACTIVE_RING.store(Some(ring.clone()));
    engine.state.begin_connecting();
    let stop = CancellationToken::new();
    let task = engine.runtime.spawn(client::run_session(
        address,
        fingerprint,
        ring.clone(),
        engine.state.clone(),
        stop.clone(),
    ));
    engine.session = Some(Session { stop, task, ring });
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
        session.stop.cancel();
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
        session.stop.cancel();
        session.task.abort();
        session.ring.clear();
    }
    engine.runtime.shutdown_timeout(Duration::from_millis(500));
}

pub fn read_pcm(output: &mut [i16]) -> usize {
    let ring = ACTIVE_RING.load_full();
    fill_audio_track(ring.as_deref(), output)
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

pub fn invalid_packets() -> u64 {
    with_state(super::state::SharedState::invalid_packets)
}

pub fn underruns() -> u64 {
    ACTIVE_RING.load_full().map_or(0, |ring| ring.underruns())
}

pub fn overwritten_samples() -> u64 {
    ACTIVE_RING
        .load_full()
        .map_or(0, |ring| ring.overwritten_samples())
}

pub fn buffer_duration_ms() -> u64 {
    ACTIVE_RING.load_full().map_or(0, |ring| {
        ring.available_samples() as u64 * 1_000 / (u64::from(SAMPLE_RATE) * u64::from(CHANNELS))
    })
}

pub fn estimated_latency_ms() -> u64 {
    let rtt_half = with_state(|state| state.rtt_ms() / 2);
    // This is deliberately an estimate, not a claim about speaker output time.
    // The 50 ms term is the target jitter delay before samples enter the ring.
    rtt_half + buffer_duration_ms() + 50
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
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeInit<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jboolean {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jboolean> { Ok(initialize().is_ok() as jboolean) })
        .resolve::<ThrowRuntimeExAndDefault>()
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
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetInvalidPackets<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(invalid_packets())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeGetOverwrittenSamples<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    unowned_env
        .with_env(|_| -> jni::errors::Result<jlong> { Ok(to_jlong(overwritten_samples())) })
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

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_audiostream_NativeBridge_nativeVersion<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    unowned_env
        .with_env(|env| JString::from_str(env, env!("CARGO_PKG_VERSION")))
        .resolve::<ThrowRuntimeExAndDefault>()
}
