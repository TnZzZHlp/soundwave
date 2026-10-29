use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use audio_stream_protocol::{CalibrationAction, ControlMessage, DeviceStatus};
use tokio_util::sync::CancellationToken;

use super::registry::{ReceiverRegistry, ReceiverSnapshot};
use crate::{
    audio::{
        microphone::{Measurement, Microphone},
        transmission::AudioTransmissionGate,
    },
    loudness::{closest_step, signal_level},
};

#[derive(Clone)]
struct Setting {
    device_id: String,
    index: u16,
    minimum: u16,
    maximum: u16,
}

#[derive(Default)]
struct State {
    running: bool,
    preview: bool,
    committing: bool,
    message: String,
    cancel: CancellationToken,
    settings: Vec<Setting>,
}

#[derive(Clone, Default)]
pub struct CalibrationController {
    state: Arc<Mutex<State>>,
}

impl CalibrationController {
    pub fn load(path: &Path) -> Self {
        let controller = Self::default();
        if let Ok(text) = fs::read_to_string(path) {
            let settings = text
                .lines()
                .filter_map(|line| {
                    let parts: Vec<_> = line.split_whitespace().collect();
                    if parts.len() != 4
                        || parts[0].len() > 80
                        || !parts[0]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        return None;
                    }
                    let setting = Setting {
                        device_id: parts[0].into(),
                        index: parts[1].parse().ok()?,
                        minimum: parts[2].parse().ok()?,
                        maximum: parts[3].parse().ok()?,
                    };
                    (setting.minimum <= setting.index
                        && setting.index <= setting.maximum
                        && setting.maximum <= 512)
                        .then_some(setting)
                })
                .collect();
            controller
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .settings = settings;
        }
        controller
    }

    pub fn status(&self) -> (bool, String) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.running, state.message.clone())
    }

    pub fn can_adjust(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.running || state.preview
    }

    pub fn can_cancel(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .committing
    }

    pub fn has_settings(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settings
            .is_empty()
    }

    pub fn cancel(&self) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.committing {
            state.cancel.cancel();
        }
    }

    fn message(&self, text: impl Into<String>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).message = text.into();
    }

    pub fn apply_saved(&self, registry: &ReceiverRegistry) -> Result<Vec<(u64, u32)>> {
        ensure!(
            !self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .settings
                .is_empty(),
            "No saved calibration; preview a reference and calibrate first"
        );
        self.apply_group(registry, 100)
    }

    pub fn apply_group(
        &self,
        registry: &ReceiverRegistry,
        percent: u16,
    ) -> Result<Vec<(u64, u32)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.running, "Calibration is running");
        ensure!(
            !state.settings.is_empty(),
            "Calibrate phones before using group volume"
        );
        let mut errors = Vec::new();
        let mut requests = Vec::new();
        for receiver in registry.snapshots() {
            let Some(device) = receiver.device else {
                continue;
            };
            if device.fixed || device.failed {
                errors.push(format!(
                    "{}: system volume is unavailable or the last request failed; skipped",
                    device.name
                ));
                continue;
            }
            let calibrated = state.settings.iter().find(|s| {
                s.device_id == device.device_id
                    && s.minimum == device.minimum
                    && s.maximum == device.maximum
            });
            if !state.settings.is_empty() && (calibrated.is_none() || !device.speaker) {
                errors.push(format!(
                    "{}: no matching speaker calibration; skipped",
                    device.name
                ));
                continue;
            }
            let base = calibrated.map_or(device.maximum, |s| s.index);
            let index = device.minimum
                + ((u32::from(base - device.minimum) * u32::from(percent.min(100)) + 50) / 100)
                    as u16;
            match registry.restore_volume_request(receiver.id, index) {
                Ok(request) => requests.push((receiver.id, request)),
                Err(error) => errors.push(error.to_string()),
            }
        }
        if !errors.is_empty() {
            state.message = errors.join("; ");
        }
        Ok(requests)
    }

    pub fn start(&self, request: CalibrationRequest) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.running, "Calibration is already running");
        ensure!(
            (1..=100).contains(&request.ceiling_percent),
            "Invalid calibration ceiling"
        );
        let cancel = request.shutdown.child_token();
        state.running = true;
        state.preview = request.preview;
        state.committing = false;
        state.cancel = cancel.clone();
        state.message = "Preparing test playback...".into();
        let controller = self.clone();
        let spawned = thread::Builder::new()
            .name("soundwave-calibration".into())
            .spawn(move || {
                let result = run(&controller, &request, &cancel);
                if let Err(error) = result {
                    controller.message(format!("{error:#}"));
                }
                controller
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .running = false;
            });
        if let Err(error) = spawned {
            state.running = false;
            return Err(error.into());
        }
        Ok(())
    }
}

pub struct CalibrationRequest {
    pub reference: u64,
    pub ceiling_percent: u16,
    pub preview: bool,
    pub registry: ReceiverRegistry,
    pub transmission: AudioTransmissionGate,
    pub shutdown: CancellationToken,
    pub profile_path: PathBuf,
}

struct PlaybackPause<'a> {
    request: &'a CalibrationRequest,
}

impl Drop for PlaybackPause<'_> {
    fn drop(&mut self) {
        self.request
            .registry
            .broadcast(ControlMessage::CalibrationTone {
                request_id: 0,
                enabled: false,
            });
        self.request
            .registry
            .broadcast(ControlMessage::CalibrationSession {
                request_id: 0,
                action: CalibrationAction::Cancel,
            });
        thread::sleep(Duration::from_millis(250));
        self.request.transmission.set_calibrating(false);
    }
}

fn run(
    controller: &CalibrationController,
    request: &CalibrationRequest,
    cancel: &CancellationToken,
) -> Result<()> {
    request.transmission.set_calibrating(true);
    let _pause = PlaybackPause { request };
    request.registry.broadcast(ControlMessage::CalibrationTone {
        request_id: 0,
        enabled: false,
    });
    wait(cancel, Duration::from_millis(800))?;
    let receivers = request.registry.snapshots();
    ensure!(!receivers.is_empty(), "No connected phones");
    let reference = receivers
        .iter()
        .find(|r| r.id == request.reference)
        .context("Reference phone disconnected")?;
    let reference_device = exchange(&request.registry, reference.id, None, cancel)?;
    validate_device(&reference_device)?;
    ensure!(
        reference_device.volume > reference_device.minimum,
        "Raise the reference phone above mute before previewing"
    );
    if request.preview {
        controller.message("Reference test: adjust the phone to a comfortable loudness, then calibrate. Test stops after 5 seconds.");
        switch_tone(&request.registry, reference.id, true, cancel)?;
        wait(cancel, Duration::from_secs(5))?;
        switch_tone(&request.registry, reference.id, false, cancel)?;
        controller
            .message("Preview complete. Select Calibrate to use this loudness as the target.");
        return Ok(());
    }

    ensure!(
        receivers.len() >= 2,
        "Connect at least two phones to match loudness"
    );
    let mut originals = Vec::new();
    for receiver in &receivers {
        let status = exchange(&request.registry, receiver.id, None, cancel)?;
        validate_device(&status)?;
        ensure!(
            !originals
                .iter()
                .any(|(_, d): &(u64, DeviceStatus)| d.device_id == status.device_id),
            "Duplicate device identity; reconnect the phones"
        );
        originals.push((receiver.id, status));
    }
    let mut begun = Vec::new();
    let result = (|| {
        for (id, original) in &mut originals {
            let actual = exchange_session(&request.registry, *id, CalibrationAction::Begin, cancel)?;
            begun.push((*id, actual.clone()));
            validate_device(&actual)?;
            ensure!(actual.device_id == original.device_id && actual.minimum == original.minimum && actual.maximum == original.maximum, "Device changed before calibration started");
            *original = actual;
        }
        let reference_device = originals.iter().find(|(id, _)| *id == request.reference)
            .map(|(_, device)| device.clone()).context("Reference phone disappeared")?;
        calibrate(
            controller,
            request,
            cancel,
            &receivers,
            &originals,
            &reference_device,
        )
    })()
    .and_then(|(settings, summary)| {
        ensure!(!cancel.is_cancelled(), "Calibration cancelled");
        for (id, _) in &originals {
            let actual = exchange(&request.registry, *id, None, cancel)?;
            validate_device(&actual)?;
            if let Some(setting) = settings.iter().find(|s| s.device_id == actual.device_id) {
                ensure!(
                    actual.volume == setting.index,
                    "A device volume changed before calibration completed"
                );
            }
        }
        {
            let mut state = controller.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!cancel.is_cancelled(), "Calibration cancelled");
            state.committing = true;
            state.message = "Saving calibrated settings, then applying them to each phone...".into();
        }
        let text = settings
            .iter()
            .map(|s| format!("{} {} {} {}\n", s.device_id, s.index, s.minimum, s.maximum))
            .collect::<String>();
        if let Some(parent) = request.profile_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = request.profile_path.with_extension("tmp");
        {
            let mut file = fs::File::create(&temporary).context("Could not save calibration")?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                &windows::core::HSTRING::from(temporary.as_os_str()),
                &windows::core::HSTRING::from(request.profile_path.as_os_str()),
                windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                    | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
        }
        .context("Could not save calibration")?;
        let mut unconfirmed = Vec::new();
        let finish = CancellationToken::new();
        for (id, original) in &originals {
            let index = settings.iter().find(|s| s.device_id == original.device_id).map_or(original.volume, |s| s.index);
            let confirmed = exchange_session(&request.registry, *id, CalibrationAction::Commit { index }, &finish)
                .is_ok_and(|actual| commit_confirmed(&actual, original, index));
            if !confirmed { unconfirmed.push(original.name.clone()); }
        }
        controller.state.lock().unwrap_or_else(|e| e.into_inner()).settings = settings;
        if unconfirmed.is_empty() {
            controller.message(summary);
        } else {
            controller.message(format!("Calibrated settings saved. Apply could not be confirmed for: {}. Check their volumes or use Apply saved calibration after reconnecting.\r\n{summary}", unconfirmed.join(", ")));
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let restore = restore(&request.registry, &begun);
            bail!("{error:#}. Original volumes restored where possible. {restore}");
        }
    }
}

fn calibrate(
    controller: &CalibrationController,
    request: &CalibrationRequest,
    cancel: &CancellationToken,
    receivers: &[ReceiverSnapshot],
    originals: &[(u64, DeviceStatus)],
    reference: &DeviceStatus,
) -> Result<(Vec<Setting>, String)> {
    let mut mic = Microphone::open()?;
    controller.message(if mic.raw {
        "Measuring ambient noise with the Windows default microphone (RAW input)..."
    } else {
        "Measuring ambient noise with the Windows default microphone (processed input; disable enhancements)..."
    });
    let ambient = mic.measure(cancel)?;
    let background = ambient.level_dbfs;
    ensure!(
        ambient.spread_db <= 3.0,
        "Unstable ambient noise; keep the room quiet and disable microphone processing"
    );
    ensure!(
        !ambient.tone_detected(),
        "Calibration tone detected before the test; stop other test playback and retry"
    );
    let target = measure(
        &request.registry,
        request.reference,
        &mut mic,
        background,
        cancel,
    )
    .with_context(|| format!("Reference phone {}", reference.name))?;
    controller.message(format!(
        "Target {target:.1} dBFS (relative, not SPL). Matching phones..."
    ));
    let mut settings = vec![setting(reference)];
    let mut results = Vec::new();
    for receiver in receivers.iter().filter(|r| r.id != request.reference) {
        let original = originals
            .iter()
            .find(|(id, _)| *id == receiver.id)
            .context("Device disappeared")?
            .1
            .clone();
        let ceiling = original.minimum
            + ((u32::from(original.maximum - original.minimum)
                * u32::from(request.ceiling_percent))
                / 100) as u16;
        let mut previous: Option<(u16, f64)> = None;
        let mut best: Option<(u16, f64)> = None;
        for index in (original.minimum.saturating_add(1)..=ceiling).take(64) {
            controller.message(format!(
                "Matching {}: step {index}/{ceiling}; target {target:.1} dBFS",
                original.name
            ));
            let actual = exchange(&request.registry, receiver.id, Some(index), cancel)?;
            validate_device(&actual)?;
            ensure!(
                actual.maximum == original.maximum
                    && actual.minimum == original.minimum
                    && actual.device_id == original.device_id,
                "Device identity or volume range changed during calibration"
            );
            let measurement =
                measure_raw(&request.registry, receiver.id, &mut mic, background, cancel)
                    .with_context(|| format!("Measuring {}", original.name))?;
            let level = match signal_level(measurement.signal.level_dbfs, measurement.background) {
                Ok(level) => level,
                Err(_) => continue,
            };
            ensure!(
                measurement.signal.tone_detected(),
                "{}: test signal not detected ({}). Check the Windows default input and disable microphone noise suppression/enhancements",
                original.name,
                measurement.signal.diagnostics(measurement.background)
            );
            ensure!(
                measurement.signal.spread_db <= 3.0,
                "Unstable sound or microphone processing; disable AGC/enhancements and retry"
            );
            if let Some(previous) = previous {
                ensure!(
                    level >= previous.1 - 2.0,
                    "Volume measurements are not monotonic; check microphone processing and room noise"
                );
            }
            let candidate = (index, level);
            best = Some(best.map_or(candidate, |old| closest_step(old, candidate, target)));
            previous = Some(candidate);
            if level >= target || (level - target).abs() <= 1.0 {
                break;
            }
        }
        let Some((index, _)) = best else {
            exchange(
                &request.registry,
                receiver.id,
                Some(original.volume),
                cancel,
            )?;
            results.push(format!(
                "{}: target unreachable within the ceiling; original volume restored",
                original.name
            ));
            continue;
        };
        let actual = exchange(&request.registry, receiver.id, Some(index), cancel)?;
        let verified = measure(&request.registry, receiver.id, &mut mic, background, cancel)?;
        if (verified - target).abs() <= 2.0 {
            settings.push(setting(&actual));
            results.push(format!(
                "{}: matched at {index}/{} ({:+.1} dB)",
                original.name,
                original.maximum,
                verified - target
            ));
        } else {
            exchange(
                &request.registry,
                receiver.id,
                Some(original.volume),
                cancel,
            )?;
            results.push(format!(
                "{}: target unreachable/coarse volume steps ({:+.1} dB); original volume restored",
                original.name,
                verified - target
            ));
        }
    }
    let reference_now = exchange(&request.registry, request.reference, None, cancel)?;
    ensure!(
        reference_now.volume == reference.volume,
        "Reference volume changed during calibration"
    );
    let repeated = measure(
        &request.registry,
        request.reference,
        &mut mic,
        background,
        cancel,
    )?;
    ensure!(
        (repeated - target).abs() <= 2.0,
        "Reference drifted; keep microphone gain and position fixed and disable AGC"
    );
    let ending_noise = mic.measure(cancel)?;
    ensure!(
        !ending_noise.tone_detected(),
        "Test playback did not stop; calibration rejected"
    );
    ensure!(
        ending_noise.level_dbfs <= background + 3.0,
        "Ambient noise changed during calibration; retry in a quiet room"
    );
    Ok((
        settings,
        format!(
            "Calibration complete (relative, within 2 dB at this position).\r\n{}",
            results.join("\r\n")
        ),
    ))
}

fn commit_confirmed(actual: &DeviceStatus, original: &DeviceStatus, index: u16) -> bool {
    !actual.failed
        && !actual.fixed
        && actual.speaker
        && actual.device_id == original.device_id
        && actual.minimum == original.minimum
        && actual.maximum == original.maximum
        && actual.volume == index
}

fn setting(device: &DeviceStatus) -> Setting {
    Setting {
        device_id: device.device_id.clone(),
        index: device.volume,
        minimum: device.minimum,
        maximum: device.maximum,
    }
}

fn validate_device(device: &DeviceStatus) -> Result<()> {
    ensure!(
        device.maximum > device.minimum,
        "{} has no adjustable volume range",
        device.name
    );
    ensure!(
        !device.failed && !device.fixed,
        "{} cannot change system media volume",
        device.name
    );
    ensure!(
        device.speaker,
        "{}: disconnect headphones, Bluetooth and external outputs before calibration",
        device.name
    );
    ensure!(
        !device.device_id.is_empty(),
        "Device has no stable identity"
    );
    Ok(())
}

struct ToneMeasurement {
    signal: Measurement,
    background: f64,
}

fn switch_tone(
    registry: &ReceiverRegistry,
    id: u64,
    enabled: bool,
    cancel: &CancellationToken,
) -> Result<u32> {
    let request_id = registry.tone_request(id, enabled)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        ensure!(!cancel.is_cancelled(), "Calibration cancelled");
        if let Some((playing, failed)) = registry.take_tone_reply(id, request_id) {
            ensure!(
                !failed && playing == enabled,
                "Phone could not {} test playback",
                if enabled { "start" } else { "stop" }
            );
            return Ok(request_id);
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for the phone to {} test playback",
            if enabled { "start" } else { "stop" }
        );
        ensure!(
            registry.snapshots().iter().any(|r| r.id == id),
            "Device disconnected"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn measure_raw(
    registry: &ReceiverRegistry,
    id: u64,
    mic: &mut Microphone,
    background: f64,
    cancel: &CancellationToken,
) -> Result<ToneMeasurement> {
    let status = exchange(registry, id, None, cancel)?;
    validate_device(&status)?;
    let started = switch_tone(registry, id, true, cancel)?;
    let result = mic.measure(cancel);
    let interrupted = registry
        .take_tone_reply(id, started)
        .is_some_and(|(playing, failed)| !playing || failed);
    let stopped = switch_tone(registry, id, false, &CancellationToken::new());
    let signal = result?;
    stopped?;
    ensure!(
        !interrupted,
        "Phone interrupted test playback during measurement"
    );
    let after = exchange(registry, id, None, cancel)?;
    validate_device(&after)?;
    ensure!(
        after.volume == status.volume,
        "Device volume changed during measurement"
    );
    let ambient = mic.measure(cancel)?;
    ensure!(
        !ambient.tone_detected(),
        "Test-like sound remains after playback stopped; silence other audio and retry"
    );
    ensure!(
        ambient.spread_db <= 3.0 && (ambient.level_dbfs - background).abs() <= 3.0,
        "Background changed during the test (initial {:.1} dBFS, current {:.1} dBFS, variation {:.1} dB); keep the room quiet and disable microphone processing",
        background,
        ambient.level_dbfs,
        ambient.spread_db
    );
    Ok(ToneMeasurement {
        signal,
        background: ambient.level_dbfs,
    })
}

fn measure(
    registry: &ReceiverRegistry,
    id: u64,
    mic: &mut Microphone,
    background: f64,
    cancel: &CancellationToken,
) -> Result<f64> {
    let measurement = measure_raw(registry, id, mic, background, cancel)?;
    ensure!(
        measurement.signal.tone_detected(),
        "Test signal not detected ({}). Check the Windows default input and disable microphone noise suppression/enhancements",
        measurement.signal.diagnostics(measurement.background)
    );
    ensure!(
        measurement.signal.spread_db <= 3.0,
        "Unstable microphone measurement; disable AGC/enhancements and keep the room quiet"
    );
    signal_level(measurement.signal.level_dbfs, measurement.background).map_err(anyhow::Error::msg)
}

fn exchange(
    registry: &ReceiverRegistry,
    id: u64,
    index: Option<u16>,
    cancel: &CancellationToken,
) -> Result<DeviceStatus> {
    let request_id = match index {
        Some(index) => registry.calibration_volume_request(id, index)?,
        None => registry.request(id, None)?,
    };
    await_reply(registry, id, request_id, cancel)
}

fn exchange_session(
    registry: &ReceiverRegistry,
    id: u64,
    action: CalibrationAction,
    cancel: &CancellationToken,
) -> Result<DeviceStatus> {
    let request_id = registry.calibration_request(id, action)?;
    await_reply(registry, id, request_id, cancel)
}

fn await_reply(
    registry: &ReceiverRegistry,
    id: u64,
    request_id: u32,
    cancel: &CancellationToken,
) -> Result<DeviceStatus> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        ensure!(!cancel.is_cancelled(), "Calibration cancelled");
        if let Some(status) = registry.take_reply(id, request_id) {
            ensure!(
                !status.failed,
                "{} rejected the volume request (system policy or safe-volume limit)",
                status.name
            );
            return Ok(status);
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for device acknowledgement"
        );
        ensure!(
            registry.snapshots().iter().any(|r| r.id == id),
            "Device disconnected"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait(cancel: &CancellationToken, duration: Duration) -> Result<()> {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        ensure!(!cancel.is_cancelled(), "Calibration cancelled");
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn restore(registry: &ReceiverRegistry, originals: &[(u64, DeviceStatus)]) -> String {
    registry.broadcast(ControlMessage::CalibrationTone {
        request_id: 0,
        enabled: false,
    });
    let cancel = CancellationToken::new();
    let mut failures = Vec::new();
    for (id, status) in originals {
        let restored = (|| -> Result<()> {
            let stopped = switch_tone(registry, *id, false, &cancel);
            let actual = exchange_session(registry, *id, CalibrationAction::Cancel, &cancel)?;
            stopped?;
            ensure!(
                commit_confirmed(&actual, status, status.volume),
                "Original volume could not be confirmed"
            );
            Ok(())
        })();
        if restored.is_err() {
            failures.push(format!(
                "Restore {} manually to {}/{}",
                status.name, status.volume, status.maximum
            ));
        }
    }
    failures.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_rejects_a_receipt_for_rolled_back_volume() {
        let original = DeviceStatus {
            request_id: 0,
            device_id: "phone".into(),
            name: "Phone".into(),
            volume: 3,
            minimum: 0,
            maximum: 15,
            fixed: false,
            speaker: true,
            failed: false,
        };
        assert!(!commit_confirmed(&original, &original, 5));
    }

    #[test]
    fn group_volume_skips_uncalibrated_phone_without_relocking_state() {
        let controller = CalibrationController::default();
        controller.state.lock().unwrap().settings.push(Setting {
            device_id: "calibrated".into(),
            index: 5,
            minimum: 0,
            maximum: 15,
        });
        let registry = ReceiverRegistry::default();
        let (sender, _receiver) = tokio::sync::mpsc::channel(16);
        let guard = registry.register("127.0.0.1:1234".parse().unwrap(), sender);
        registry.update_device(
            guard.id(),
            DeviceStatus {
                request_id: 0,
                device_id: "new-phone".into(),
                name: "New phone".into(),
                volume: 3,
                minimum: 0,
                maximum: 15,
                fixed: false,
                speaker: true,
                failed: false,
            },
        );
        assert!(controller.apply_group(&registry, 100).unwrap().is_empty());
        assert!(controller.status().1.contains("skipped"));
    }
}
