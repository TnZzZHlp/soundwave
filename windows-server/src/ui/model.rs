use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use crate::application::{
    calibration::CalibrationRequest, registry::ReceiverSnapshot, state::ServerState,
    volume_control::VolumeView,
};
use anyhow::{Context, Result, ensure};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Receivers,
    Information,
    Settings,
}

#[derive(Clone)]
pub(super) enum Command {
    Show(Page),
    Toggle,
    AdjustAll(i8),
    OpenCalibration,
    CloseCalibration,
    SetMaster(u64),
    DeviceVolume(u64, u16),
    MasterPoint(u16),
    MasterPosition(u16),
    Record(u64),
    RemovePoint(u64),
    PreviewDevice(u64),
    PreviewPair(u64),
    StopTone,
    Calibrate,
    Cancel,
    Profile(u64),
    NewProfile,
    SaveProfile(String, bool),
    DeleteProfile,
    Exit,
    CopyPairing,
    CopyFingerprint,
}

pub(super) struct Controller {
    pub server: ServerState,
    pub receivers: Vec<ReceiverSnapshot>,
    pub page: Page,
    pub point: u16,
    pub notice: String,
    pending: BTreeMap<u64, (u16, Instant)>,
    replies: BTreeMap<u64, ManualRequest>,
    sessions: BTreeSet<u64>,
}

struct ManualRequest {
    request: u32,
    index: u16,
    at: Instant,
    beginning: bool,
}

impl Controller {
    pub fn new(server: ServerState) -> Self {
        let mut controller = Self {
            server,
            receivers: Vec::new(),
            page: Page::Receivers,
            point: 0,
            notice: String::new(),
            pending: BTreeMap::new(),
            replies: BTreeMap::new(),
            sessions: BTreeSet::new(),
        };
        controller.refresh();
        controller
    }

    pub fn view(&self) -> VolumeView {
        self.server.volume.view()
    }

    pub fn refresh(&mut self) {
        self.receivers = self.server.registry.snapshots();
        if let Some(profile) = self.server.calibration.take_result()
            && let Err(error) = self.server.volume.set_draft(profile)
        {
            self.notice = error.to_string();
        }
        self.sessions
            .retain(|id| self.receivers.iter().any(|r| r.id == *id));
        self.replies.retain(|id, pending| {
            let failed = if let Some(reply) = self.server.registry.take_reply(*id, pending.request)
            {
                if !reply.failed && reply.volume == pending.index && reply.speaker && !reply.fixed {
                    return false;
                }
                self.notice = format!("{} 未能确认校准音量操作", reply.name);
                true
            } else if pending.at.elapsed() >= Duration::from_secs(4)
                || !self.receivers.iter().any(|r| r.id == *id)
            {
                self.notice = "校准音量操作未收到确认，请检查手机连接".into();
                true
            } else {
                false
            };
            if failed {
                self.pending.remove(id);
                if pending.beginning {
                    self.sessions.remove(id);
                }
                false
            } else {
                true
            }
        });
        if self.server.calibration.status().0 {
            return;
        }
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter(|(id, (_, at))| {
                at.elapsed() >= Duration::from_millis(120) && !self.replies.contains_key(id)
            })
            .map(|(id, (index, _))| (*id, *index))
            .collect();
        for (id, index) in ready {
            let beginning = !self.sessions.contains(&id);
            let result = if beginning {
                self.server
                    .registry
                    .calibration_request(id, audio_stream_protocol::CalibrationAction::Begin)
            } else {
                self.pending.remove(&id);
                self.server.registry.calibration_volume_request(id, index)
            };
            match result {
                Ok(request) => {
                    let expected = if beginning {
                        self.sessions.insert(id);
                        self.receivers
                            .iter()
                            .find(|r| r.id == id)
                            .and_then(|r| r.device.as_ref())
                            .map_or(index, |d| d.volume)
                    } else {
                        index
                    };
                    self.replies.insert(
                        id,
                        ManualRequest {
                            request,
                            index: expected,
                            at: Instant::now(),
                            beginning,
                        },
                    );
                }
                Err(error) => {
                    self.pending.remove(&id);
                    self.notice = error.to_string();
                }
            }
        }
    }

    pub fn volume(&self, receiver: &ReceiverSnapshot) -> u16 {
        self.pending
            .get(&receiver.id)
            .map(|(index, _)| *index)
            .or_else(|| {
                self.replies
                    .get(&receiver.id)
                    .filter(|r| !r.beginning)
                    .map(|r| r.index)
            })
            .or_else(|| receiver.device.as_ref().map(|d| d.volume))
            .unwrap_or(0)
    }

    pub fn settled(&self) -> bool {
        self.pending.is_empty() && self.replies.is_empty()
    }

    pub fn command(&mut self, command: Command) {
        let result = self.execute(command);
        match result {
            Ok(()) => {}
            Err(error) => self.notice = format!("{error:#}"),
        }
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        if matches!(command, Command::Cancel) {
            self.server.calibration.cancel();
            return Ok(());
        }
        if matches!(command, Command::Exit) {
            self.server.shutdown.cancel();
            return Ok(());
        }
        ensure!(
            !self.server.calibration.status().0,
            "请先等待测量结束或取消测量"
        );
        if let Some(profile) = self.server.calibration.take_result() {
            self.server.volume.set_draft(profile)?;
        }
        self.server.calibration.clear_message();
        match command {
            Command::Show(page) => self.page = page,
            Command::Toggle => {
                ensure!(!self.view().editing, "请先关闭校准窗口");
                self.server
                    .transmission
                    .set_enabled(!self.server.transmission.is_enabled());
            }
            Command::OpenCalibration => {
                self.server
                    .volume
                    .open(&self.server.registry, &self.server.transmission)?;
                self.point = self.view().draft.as_ref().map_or(0, |p| p.volume);
                self.notice.clear();
            }
            Command::CloseCalibration => {
                ensure!(self.settled(), "请等待音量设置确认后关闭窗口");
                self.server
                    .volume
                    .close(&self.server.registry, &self.server.transmission);
                self.sessions.clear();
                self.notice.clear();
            }
            Command::SetMaster(id) => {
                ensure!(self.settled(), "请等待音量设置确认");
                let device = self
                    .receivers
                    .iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.device.as_ref())
                    .context("设备尚未就绪")?;
                self.server
                    .volume
                    .set_master(device, &self.server.registry)?;
                self.point = device.volume;
            }
            Command::DeviceVolume(id, index) => {
                let view = self.view();
                if view.editing {
                    let device = self
                        .receivers
                        .iter()
                        .find(|r| r.id == id)
                        .and_then(|r| r.device.as_ref())
                        .context("设备尚未就绪")?;
                    ensure!(
                        device.speaker && !device.fixed && !device.failed,
                        "设备无法调整扬声器音量"
                    );
                    self.server.volume.remember(id, device);
                    let index = index.clamp(device.minimum, device.maximum);
                    if view
                        .draft
                        .as_ref()
                        .is_some_and(|p| p.master.id == device.device_id)
                    {
                        self.point = index;
                    }
                    self.pending.insert(id, (index, Instant::now()));
                } else {
                    let profile = view.library.current().context("请先保存主从校准配置")?;
                    ensure!(
                        self.receivers.iter().any(|r| r.id == id
                            && r.device.as_ref().is_some_and(|d| profile.master.matches(d))),
                        "正式使用时仅主设备音量可调"
                    );
                    self.server
                        .volume
                        .set_volume(&self.server.registry, index)?;
                }
                self.notice.clear();
            }
            Command::MasterPosition(position) => {
                let view = self.view();
                let profile = view.library.current().context("请先应用校准配置")?;
                let devices = self
                    .receivers
                    .iter()
                    .filter_map(|r| r.device.as_ref())
                    .collect::<Vec<_>>();
                let participants = profile.participating(&devices);
                let available = profile.available(&participants);
                let index = available
                    .get(usize::from(position))
                    .context("所选档位未校准")?;
                self.server
                    .volume
                    .set_volume(&self.server.registry, *index)?;
                self.notice.clear();
            }
            Command::MasterPoint(index) => {
                let draft = self.view().draft.context("请先指定主设备")?;
                let receiver = self
                    .receivers
                    .iter()
                    .find(|r| r.device.as_ref().is_some_and(|d| draft.master.matches(d)))
                    .context("主设备未连接")?;
                return self.execute(Command::DeviceVolume(receiver.id, index));
            }
            Command::Record(id) => {
                ensure!(
                    self.view().editing && self.settled(),
                    "请等待所有音量得到确认后保存对应点"
                );
                let device = self
                    .receivers
                    .iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.device.as_ref())
                    .context("从设备未连接")?;
                let draft = self.view().draft.context("请先指定主设备")?;
                ensure!(
                    self.receivers.iter().any(|r| r
                        .device
                        .as_ref()
                        .is_some_and(|d| draft.master.matches(d) && d.volume == self.point)),
                    "主设备当前档位与所选校准点不符"
                );
                let point = self.point;
                self.server.volume.edit(|profile| {
                    let follower = profile
                        .followers
                        .iter_mut()
                        .find(|f| f.device.matches(device))
                        .context("从设备范围已变化，请重新指定主设备")?;
                    follower.mapping.insert(point, device.volume);
                    profile.volume = point;
                    Ok(())
                })?;
                self.notice = format!(
                    "已记录：主设备 {} → {} {}",
                    self.point, device.name, device.volume
                );
            }
            Command::RemovePoint(id) => {
                let device = self
                    .receivers
                    .iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.device.as_ref())
                    .context("设备未连接")?;
                self.server.volume.edit(|profile| {
                    let follower = profile
                        .followers
                        .iter_mut()
                        .find(|f| f.device.id == device.device_id)
                        .context("请选择从设备")?;
                    follower.mapping.remove(&self.point);
                    Ok(())
                })?;
            }
            Command::PreviewDevice(id) | Command::PreviewPair(id) => {
                let view = self.view();
                ensure!(view.editing && self.settled(), "请等待音量确认后试听");
                let mut targets = vec![id];
                if matches!(command, Command::PreviewPair(_)) {
                    let profile = view.draft.context("请先指定主设备")?;
                    let follower = self
                        .receivers
                        .iter()
                        .find(|r| r.id == id)
                        .and_then(|r| r.device.as_ref())
                        .context("从设备未连接")?;
                    ensure!(
                        profile.followers.iter().any(|f| f.device.matches(follower)),
                        "从设备当前不可用"
                    );
                    let master = self
                        .receivers
                        .iter()
                        .find(|r| r.device.as_ref().is_some_and(|d| profile.master.matches(d)))
                        .context("主设备未连接或当前不可用")?;
                    targets.push(master.id);
                }
                self.server.registry.broadcast(
                    audio_stream_protocol::ControlMessage::CalibrationTone {
                        request_id: 0,
                        enabled: false,
                    },
                );
                for target in targets {
                    if let Err(error) = self.server.registry.tone_request(target, true) {
                        self.server.registry.broadcast(
                            audio_stream_protocol::ControlMessage::CalibrationTone {
                                request_id: 0,
                                enabled: false,
                            },
                        );
                        return Err(error);
                    }
                }
            }
            Command::StopTone => self.server.registry.broadcast(
                audio_stream_protocol::ControlMessage::CalibrationTone {
                    request_id: 0,
                    enabled: false,
                },
            ),
            Command::Calibrate => {
                ensure!(
                    self.view().editing && self.settled(),
                    "请等待音量确认后测量"
                );
                let profile = self.view().draft.context("请先指定主设备")?;
                let reference = self
                    .receivers
                    .iter()
                    .find(|r| r.device.as_ref().is_some_and(|d| profile.master.matches(d)))
                    .context("主设备未连接")?
                    .id;
                self.server.registry.broadcast(
                    audio_stream_protocol::ControlMessage::CalibrationTone {
                        request_id: 0,
                        enabled: false,
                    },
                );
                self.server.registry.broadcast(
                    audio_stream_protocol::ControlMessage::CalibrationSession {
                        request_id: 0,
                        action: audio_stream_protocol::CalibrationAction::Cancel,
                    },
                );
                self.sessions.clear();
                self.server.calibration.start(CalibrationRequest {
                    reference,
                    profile,
                    registry: self.server.registry.clone(),
                    shutdown: self.server.shutdown.clone(),
                })?;
                self.notice.clear();
            }
            Command::Profile(id) => {
                ensure!(self.settled(), "请等待音量设置确认");
                self.server.volume.select(id, &self.server.registry)?;
                for receiver in &self.receivers {
                    let _ = self.server.registry.request(receiver.id, None);
                }
                self.point = self.view().draft.as_ref().map_or(0, |p| p.volume);
                self.notice.clear();
            }
            Command::NewProfile => {
                self.server.volume.new_draft()?;
                self.point = 0;
            }
            Command::SaveProfile(name, copy) => {
                ensure!(self.settled(), "请等待音量设置确认");
                self.server.volume.edit(|profile| {
                    profile.volume = self.point;
                    Ok(())
                })?;
                self.server.volume.save_draft(name, copy)?;
                self.notice = "配置已保存；关闭校准窗口后使用保存的对应关系".into();
            }
            Command::DeleteProfile => {
                if let Some(draft) = self.view().draft {
                    self.server.volume.delete(draft.id)?;
                }
            }
            Command::AdjustAll(steps) => {
                let profile = self
                    .view()
                    .library
                    .current()
                    .cloned()
                    .context("请先保存主从校准配置")?;
                let snapshots = self.server.registry.snapshots();
                let devices = snapshots
                    .iter()
                    .filter_map(|r| r.device.as_ref())
                    .collect::<Vec<_>>();
                let participants = profile.participating(&devices);
                let available = profile.available(&participants);
                let next = if steps > 0 {
                    available.into_iter().find(|index| *index > profile.volume)
                } else {
                    available
                        .into_iter()
                        .rev()
                        .find(|index| *index < profile.volume)
                };
                if let Some(index) = next {
                    self.server
                        .volume
                        .set_volume(&self.server.registry, index)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
