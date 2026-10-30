use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use audio_stream_protocol::{CalibrationAction, ControlMessage, DeviceStatus};
use tokio_util::sync::CancellationToken;

use super::registry::ReceiverRegistry;
use crate::{
    audio::microphone::{Measurement, Microphone},
    loudness::{closest_step, signal_level},
    volume_profiles::Profile,
};

#[derive(Default)]
struct State {
    running: bool,
    message: String,
    cancel: CancellationToken,
    result: Option<Profile>,
}

#[derive(Clone, Default)]
pub struct CalibrationController {
    state: Arc<Mutex<State>>,
}

impl CalibrationController {
    pub fn status(&self) -> (bool, String) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.running, state.message.clone())
    }

    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancel.cancel();
        if state.result.take().is_some() {
            state.message = "校准已取消，原音量已恢复".into();
        }
    }

    pub fn clear_message(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .message
            .clear();
    }

    pub fn take_result(&self) -> Option<Profile> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .take()
    }

    fn message(&self, text: impl Into<String>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).message = text.into();
    }

    pub fn start(&self, request: CalibrationRequest) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.running, "校准已在进行中");
        let cancel = request.shutdown.child_token();
        state.running = true;
        state.cancel = cancel.clone();
        state.result = None;
        state.message = "正在准备校准…".into();
        let controller = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("soundwave-calibration".into())
            .spawn(move || {
                let result = run(&controller, request, &cancel);
                let mut state = controller.state.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(_) if cancel.is_cancelled() => {
                        state.message = "校准已取消，原音量已恢复".into();
                    }
                    Ok(profile) => {
                        state.result = Some(profile);
                        state.message = "测量结束，原音量已恢复。请检查对应表后保存配置。".into();
                    }
                    Err(error) => state.message = format!("{error:#}"),
                }
                state.running = false;
            })
        {
            state.running = false;
            return Err(error.into());
        }
        Ok(())
    }
}

pub struct CalibrationRequest {
    pub reference: u64,
    pub profile: Profile,
    pub registry: ReceiverRegistry,
    pub shutdown: CancellationToken,
}

fn run(
    controller: &CalibrationController,
    request: CalibrationRequest,
    cancel: &CancellationToken,
) -> Result<Profile> {
    request.profile.validate()?;
    let snapshots = request.registry.snapshots();
    let mut originals = Vec::new();
    for expected in std::iter::once(&request.profile.master)
        .chain(request.profile.followers.iter().map(|f| &f.device))
    {
        let matches: Vec<_> = snapshots
            .iter()
            .filter(|r| {
                r.device
                    .as_ref()
                    .is_some_and(|d| d.device_id == expected.id)
            })
            .collect();
        ensure!(matches.len() == 1, "{} 未连接或设备标识重复", expected.name);
        let receiver = matches[0];
        let status = exchange(&request.registry, receiver.id, None, cancel)?;
        validate_device(&status)?;
        ensure!(
            expected.matches(&status),
            "{} 的音量范围或输出已变化，请重新指定主设备",
            expected.name
        );
        originals.push((receiver.id, status));
    }
    ensure!(
        originals.iter().any(|(id, device)| *id == request.reference
            && device.device_id == request.profile.master.id),
        "主设备已断开连接"
    );
    let mut begun = Vec::new();
    let outcome = (|| {
        for (id, original) in &originals {
            let request_id = request
                .registry
                .calibration_request(*id, CalibrationAction::Begin)?;
            begun.push((*id, original.clone()));
            let actual = await_reply(&request.registry, *id, request_id, cancel)?;
            ensure!(
                commit_confirmed(&actual, original, original.volume),
                "校准开始前设备发生变化"
            );
        }
        ensure!(
            !request.profile.followers.is_empty(),
            "请至少连接一台从设备"
        );
        let mut mic = Microphone::open()?;
        controller.message("正在测量环境噪声，请保持安静并关闭麦克风自动增益。");
        let ambient = mic.measure(cancel)?;
        ensure!(
            ambient.spread_db <= 3.0 && !ambient.tone_detected(),
            "环境噪声不稳定或检测到其他测试音"
        );
        let mut curves = Vec::new();
        for (id, device) in &originals {
            let mut points = Vec::new();
            for index in device.minimum.max(1)..=device.maximum {
                controller.message(format!(
                    "正在测量 {}：{index}/{}（允许完整系统音量范围）",
                    device.name, device.maximum
                ));
                let actual = exchange(&request.registry, *id, Some(index), cancel)?;
                ensure!(
                    commit_confirmed(&actual, device, index),
                    "设备拒绝了档位或音量范围发生变化"
                );
                let measured =
                    measure_raw(&request.registry, *id, &mut mic, ambient.level_dbfs, cancel)?;
                if measured.signal.tone_detected()
                    && measured.signal.spread_db <= 3.0
                    && let Ok(level) = signal_level(measured.signal.level_dbfs, measured.background)
                {
                    points.push((index, level));
                }
            }
            curves.push((device.device_id.clone(), points));
        }
        let targets = &curves
            .iter()
            .find(|(id, _)| *id == request.profile.master.id)
            .context("没有主设备测量结果")?
            .1;
        ensure!(
            !targets.is_empty(),
            "主设备测试音无法可靠测量，请改用手动校准或检查麦克风"
        );
        let mut profile = request.profile.clone();
        for follower in &mut profile.followers {
            follower.mapping.clear();
            if profile.master.minimum == 0 && follower.device.minimum == 0 {
                follower.mapping.insert(0, 0);
            }
            let points = &curves
                .iter()
                .find(|(id, _)| *id == follower.device.id)
                .context("从设备测量缺失")?
                .1;
            let id = originals
                .iter()
                .find(|(_, d)| d.device_id == follower.device.id)
                .unwrap()
                .0;
            for (master_index, target) in targets {
                let best = points
                    .iter()
                    .copied()
                    .reduce(|a, b| closest_step(a, b, *target));
                let Some((index, level)) = best else {
                    continue;
                };
                if (level - target).abs() > 2.0 {
                    continue;
                }
                controller.message(format!(
                    "正在复核 {}：主设备 {master_index} → 从设备 {index}",
                    follower.device.name
                ));
                exchange(
                    &request.registry,
                    request.reference,
                    Some(*master_index),
                    cancel,
                )?;
                let repeated = measure(
                    &request.registry,
                    request.reference,
                    &mut mic,
                    ambient.level_dbfs,
                    cancel,
                )?;
                ensure!(
                    (repeated - target).abs() <= 2.0,
                    "主设备响度发生漂移，请固定麦克风位置并关闭自动增益"
                );
                exchange(&request.registry, id, Some(index), cancel)?;
                let verified =
                    measure(&request.registry, id, &mut mic, ambient.level_dbfs, cancel)?;
                if (verified - repeated).abs() <= 2.0 {
                    follower.mapping.insert(*master_index, index);
                }
            }
        }
        profile.validate()?;
        ensure!(
            profile.followers.iter().any(|f| f
                .mapping
                .keys()
                .any(|index| *index > profile.master.minimum)),
            "未找到可靠的对应档位，请使用手动校准"
        );
        Ok(profile)
    })();
    controller.message("正在停止测试并恢复校准前音量…");
    let restored = restore(&request.registry, &begun);
    if !restored.is_empty() {
        let cause = outcome
            .as_ref()
            .err()
            .map_or_else(String::new, |error| format!("{error:#}。"));
        bail!("校准未提交：{cause}{restored}");
    }
    outcome
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
    use crate::volume_profiles::DeviceRange;

    #[test]
    fn cancel_after_begin_sent_cleans_up_unconfirmed_session() {
        let registry = ReceiverRegistry::default();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let guard = registry.register("127.0.0.1:1001".parse().unwrap(), sender);
        let device = DeviceStatus {
            request_id: 0,
            device_id: "master".into(),
            name: "Master".into(),
            volume: 3,
            minimum: 0,
            maximum: 15,
            fixed: false,
            speaker: true,
            failed: false,
        };
        registry.update_device(guard.id(), device.clone());
        let cancel = CancellationToken::new();
        let request = CalibrationRequest {
            reference: guard.id(),
            profile: Profile {
                id: 1,
                name: "Room".into(),
                master: DeviceRange::from_status(&device),
                followers: Vec::new(),
                volume: 3,
            },
            registry: registry.clone(),
            shutdown: CancellationToken::new(),
        };
        let child_cancel = cancel.clone();
        let worker =
            thread::spawn(move || run(&CalibrationController::default(), request, &child_cancel));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut began = false;
        let mut cleaned = false;
        while Instant::now() < deadline && !cleaned {
            match receiver.try_recv() {
                Ok(ControlMessage::QueryDevice { request_id }) => {
                    registry.update_device(
                        guard.id(),
                        DeviceStatus {
                            request_id,
                            ..device.clone()
                        },
                    );
                }
                Ok(ControlMessage::CalibrationSession {
                    request_id,
                    action: CalibrationAction::Begin,
                }) => {
                    began = true;
                    cancel.cancel();
                    registry.update_device(
                        guard.id(),
                        DeviceStatus {
                            request_id,
                            ..device.clone()
                        },
                    );
                }
                Ok(ControlMessage::CalibrationTone {
                    request_id,
                    enabled: false,
                }) if request_id != 0 => {
                    registry.update_tone(guard.id(), request_id, false, false);
                }
                Ok(ControlMessage::CalibrationSession {
                    request_id,
                    action: CalibrationAction::Cancel,
                }) => {
                    assert!(began);
                    registry.update_device(
                        guard.id(),
                        DeviceStatus {
                            request_id,
                            ..device.clone()
                        },
                    );
                    cleaned = true;
                }
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        assert!(cleaned, "unconfirmed Begin must be followed by Cancel");
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn cancel_revokes_published_unclaimed_result() {
        let controller = CalibrationController::default();
        controller.state.lock().unwrap().result = Some(Profile {
            id: 1,
            name: "Room".into(),
            master: DeviceRange {
                id: "master".into(),
                name: "Master".into(),
                minimum: 0,
                maximum: 15,
            },
            followers: Vec::new(),
            volume: 3,
        });
        controller.cancel();
        assert!(controller.take_result().is_none());
    }
}
