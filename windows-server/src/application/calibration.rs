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
            "尚未保存校准，请先试听基准手机并完成校准"
        );
        self.apply_group(registry, 100)
    }

    pub fn apply_group(
        &self,
        registry: &ReceiverRegistry,
        percent: u16,
    ) -> Result<Vec<(u64, u32)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.running, "正在校准");
        ensure!(!state.settings.is_empty(), "使用群组音量前，请先校准手机");
        let mut errors = Vec::new();
        let mut requests = Vec::new();
        for receiver in registry.snapshots() {
            let Some(device) = receiver.device else {
                continue;
            };
            if device.fixed || device.failed {
                errors.push(format!(
                    "{}：系统音量不可用或上次请求失败，已跳过",
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
                errors.push(format!("{}：没有匹配的扬声器校准记录，已跳过", device.name));
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
        ensure!(!state.running, "校准已在进行中");
        ensure!(
            (1..=100).contains(&request.ceiling_percent),
            "校准音量上限无效"
        );
        let cancel = request.shutdown.child_token();
        state.running = true;
        state.preview = request.preview;
        state.committing = false;
        state.cancel = cancel.clone();
        state.message = "正在准备试听…".into();
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
    ensure!(!receivers.is_empty(), "没有已连接的手机");
    let reference = receivers
        .iter()
        .find(|r| r.id == request.reference)
        .context("基准手机已断开连接")?;
    let reference_device = exchange(&request.registry, reference.id, None, cancel)?;
    validate_device(&reference_device)?;
    ensure!(
        reference_device.volume > reference_device.minimum,
        "请先调高基准手机音量，取消静音后再试听"
    );
    if request.preview {
        controller.message("基准试听：请将手机调至舒适音量，然后开始校准。试听将在 5 秒后停止。");
        switch_tone(&request.registry, reference.id, true, cancel)?;
        wait(cancel, Duration::from_secs(5))?;
        switch_tone(&request.registry, reference.id, false, cancel)?;
        controller.message("试听结束。点击“校准”，以当前响度为目标。");
        return Ok(());
    }

    ensure!(receivers.len() >= 2, "请至少连接两台手机以匹配响度");
    let mut originals = Vec::new();
    for receiver in &receivers {
        let status = exchange(&request.registry, receiver.id, None, cancel)?;
        validate_device(&status)?;
        ensure!(
            !originals
                .iter()
                .any(|(_, d): &(u64, DeviceStatus)| d.device_id == status.device_id),
            "设备标识重复，请重新连接手机"
        );
        originals.push((receiver.id, status));
    }
    let mut begun = Vec::new();
    let result = (|| {
        for (id, original) in &mut originals {
            let actual = exchange_session(&request.registry, *id, CalibrationAction::Begin, cancel)?;
            begun.push((*id, actual.clone()));
            validate_device(&actual)?;
            ensure!(actual.device_id == original.device_id && actual.minimum == original.minimum && actual.maximum == original.maximum, "校准开始前设备已发生变化");
            *original = actual;
        }
        let reference_device = originals.iter().find(|(id, _)| *id == request.reference)
            .map(|(_, device)| device.clone()).context("基准手机已断开连接")?;
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
        ensure!(!cancel.is_cancelled(), "校准已取消");
        for (id, _) in &originals {
            let actual = exchange(&request.registry, *id, None, cancel)?;
            validate_device(&actual)?;
            if let Some(setting) = settings.iter().find(|s| s.device_id == actual.device_id) {
                ensure!(
                    actual.volume == setting.index,
                    "校准完成前，有设备的音量发生了变化"
                );
            }
        }
        {
            let mut state = controller.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!cancel.is_cancelled(), "校准已取消");
            state.committing = true;
            state.message = "正在保存校准设置，随后将应用到各台手机…".into();
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
            let mut file = fs::File::create(&temporary).context("无法保存校准设置")?;
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
        .context("无法保存校准设置")?;
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
            controller.message(format!("校准设置已保存，但以下设备未确认应用结果：{}。请检查音量，或重新连接后点击“应用已保存的校准”。\r\n{summary}", unconfirmed.join(", ")));
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let restore = restore(&request.registry, &begun);
            bail!("{error:#}。已尽可能恢复原始音量。{restore}");
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
        "正在使用 Windows 默认麦克风测量环境噪声（原始输入）…"
    } else {
        "正在使用 Windows 默认麦克风测量环境噪声（已处理的输入，请关闭音效增强）…"
    });
    let ambient = mic.measure(cancel)?;
    let background = ambient.level_dbfs;
    ensure!(
        ambient.spread_db <= 3.0,
        "环境噪声不稳定，请保持室内安静，并关闭麦克风音频处理"
    );
    ensure!(
        !ambient.tone_detected(),
        "测试开始前检测到校准音，请停止其他测试音频后重试"
    );
    let target = measure(
        &request.registry,
        request.reference,
        &mut mic,
        background,
        cancel,
    )
    .with_context(|| format!("基准手机 {}", reference.name))?;
    controller.message(format!(
        "目标响度为 {target:.1} dBFS（相对值，非声压级），正在匹配手机…"
    ));
    let mut settings = vec![setting(reference)];
    let mut results = Vec::new();
    for receiver in receivers.iter().filter(|r| r.id != request.reference) {
        let original = originals
            .iter()
            .find(|(id, _)| *id == receiver.id)
            .context("设备已断开连接")?
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
                "正在匹配 {}：音量档位 {index}/{ceiling}，目标 {target:.1} dBFS",
                original.name
            ));
            let actual = exchange(&request.registry, receiver.id, Some(index), cancel)?;
            validate_device(&actual)?;
            ensure!(
                actual.maximum == original.maximum
                    && actual.minimum == original.minimum
                    && actual.device_id == original.device_id,
                "校准过程中，设备标识或音量范围发生了变化"
            );
            let measurement =
                measure_raw(&request.registry, receiver.id, &mut mic, background, cancel)
                    .with_context(|| format!("正在测量 {}", original.name))?;
            let level = match signal_level(measurement.signal.level_dbfs, measurement.background) {
                Ok(level) => level,
                Err(_) => continue,
            };
            ensure!(
                measurement.signal.tone_detected(),
                "{}：未检测到测试信号（{}）。请检查 Windows 默认输入设备，并关闭麦克风降噪和音效增强",
                original.name,
                measurement.signal.diagnostics(measurement.background)
            );
            ensure!(
                measurement.signal.spread_db <= 3.0,
                "声音不稳定或受麦克风音频处理影响，请关闭自动增益和音效增强后重试"
            );
            if let Some(previous) = previous {
                ensure!(
                    level >= previous.1 - 2.0,
                    "音量测量结果未随档位递增，请检查麦克风音频处理和室内噪声"
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
                "{}：在音量上限内无法达到目标响度，已恢复原始音量",
                original.name
            ));
            continue;
        };
        let actual = exchange(&request.registry, receiver.id, Some(index), cancel)?;
        let verified = measure(&request.registry, receiver.id, &mut mic, background, cancel)?;
        if (verified - target).abs() <= 2.0 {
            settings.push(setting(&actual));
            results.push(format!(
                "{}：已在音量档位 {index}/{} 匹配（{:+.1} dB）",
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
                "{}：无法达到目标响度或音量档位过粗（{:+.1} dB），已恢复原始音量",
                original.name,
                verified - target
            ));
        }
    }
    let reference_now = exchange(&request.registry, request.reference, None, cancel)?;
    ensure!(
        reference_now.volume == reference.volume,
        "校准过程中基准手机音量发生了变化"
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
        "基准响度发生漂移，请固定麦克风增益和位置，并关闭自动增益"
    );
    let ending_noise = mic.measure(cancel)?;
    ensure!(
        !ending_noise.tone_detected(),
        "测试播放未停止，校准结果已拒绝"
    );
    ensure!(
        ending_noise.level_dbfs <= background + 3.0,
        "校准过程中环境噪声发生变化，请在安静环境中重试"
    );
    Ok((
        settings,
        format!(
            "校准完成（相对响度，在当前聆听位置的误差不超过 2 dB）。\r\n{}",
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
        "{} 没有可调整的音量范围",
        device.name
    );
    ensure!(
        !device.failed && !device.fixed,
        "{} 无法调整系统媒体音量",
        device.name
    );
    ensure!(
        device.speaker,
        "{}：校准前请断开耳机、蓝牙和外接音频输出设备",
        device.name
    );
    ensure!(!device.device_id.is_empty(), "设备没有稳定的标识");
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
        ensure!(!cancel.is_cancelled(), "校准已取消");
        if let Some((playing, failed)) = registry.take_tone_reply(id, request_id) {
            ensure!(
                !failed && playing == enabled,
                "手机无法{}测试播放",
                if enabled { "开始" } else { "停止" }
            );
            return Ok(request_id);
        }
        ensure!(
            Instant::now() < deadline,
            "等待手机{}测试播放超时",
            if enabled { "开始" } else { "停止" }
        );
        ensure!(
            registry.snapshots().iter().any(|r| r.id == id),
            "设备已断开连接"
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
    ensure!(!interrupted, "测量过程中手机中断了测试播放");
    let after = exchange(registry, id, None, cancel)?;
    validate_device(&after)?;
    ensure!(
        after.volume == status.volume,
        "测量过程中设备音量发生了变化"
    );
    let ambient = mic.measure(cancel)?;
    ensure!(
        !ambient.tone_detected(),
        "播放停止后仍检测到类似测试音的声音，请停止其他音频后重试"
    );
    ensure!(
        ambient.spread_db <= 3.0 && (ambient.level_dbfs - background).abs() <= 3.0,
        "测试过程中背景噪声发生变化（初始 {:.1} dBFS，当前 {:.1} dBFS，波动 {:.1} dB），请保持室内安静并关闭麦克风音频处理",
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
        "未检测到测试信号（{}）。请检查 Windows 默认输入设备，并关闭麦克风降噪和音效增强",
        measurement.signal.diagnostics(measurement.background)
    );
    ensure!(
        measurement.signal.spread_db <= 3.0,
        "麦克风测量不稳定，请关闭自动增益和音效增强，并保持室内安静"
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
        ensure!(!cancel.is_cancelled(), "校准已取消");
        if let Some(status) = registry.take_reply(id, request_id) {
            ensure!(
                !status.failed,
                "{} 拒绝了音量调整请求（系统策略或安全音量限制）",
                status.name
            );
            return Ok(status);
        }
        ensure!(Instant::now() < deadline, "等待设备确认超时");
        ensure!(
            registry.snapshots().iter().any(|r| r.id == id),
            "设备已断开连接"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait(cancel: &CancellationToken, duration: Duration) -> Result<()> {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        ensure!(!cancel.is_cancelled(), "校准已取消");
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
                "无法确认原始音量是否已恢复"
            );
            Ok(())
        })();
        if restored.is_err() {
            failures.push(format!(
                "请手动将 {} 的音量恢复至 {}/{}",
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
        assert!(controller.status().1.contains("已跳过"));
    }
}
