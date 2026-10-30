use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use audio_stream_protocol::{ControlMessage, DeviceStatus};
use tokio_util::sync::CancellationToken;

use super::registry::ReceiverRegistry;
use crate::{
    audio::transmission::AudioTransmissionGate,
    volume_profiles::{DeviceRange, Follower, Library, Profile},
};

#[derive(Clone)]
pub struct VolumeController {
    state: Arc<Mutex<State>>,
    path: PathBuf,
    worker: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

struct Pending {
    request: u32,
    index: u16,
    at: Instant,
}

struct State {
    library: Library,
    draft: Option<Profile>,
    editing: bool,
    originals: Vec<(u64, DeviceStatus)>,
    pending: BTreeMap<u64, Pending>,
    blocked: BTreeSet<u64>,
    master_seen: Option<(u64, u16)>,
    message: String,
    dirty: bool,
    changed: Instant,
    confirmed: Option<(u64, u16)>,
    load_error: Option<String>,
    needs_fence: bool,
}

impl State {
    fn new(library: Library, message: String) -> Self {
        let confirmed = library.current().map(|p| (p.id, p.volume));
        Self {
            library,
            draft: None,
            editing: false,
            originals: Vec::new(),
            pending: BTreeMap::new(),
            blocked: BTreeSet::new(),
            master_seen: None,
            message,
            dirty: false,
            changed: Instant::now(),
            confirmed,
            load_error: None,
            needs_fence: false,
        }
    }

    fn ensure_writable(&self) -> Result<()> {
        if let Some(error) = &self.load_error {
            anyhow::bail!("音量配置无法读取，已禁止覆盖原文件：{error}");
        }
        Ok(())
    }

    fn persisted(&self) -> Library {
        let mut library = self.library.clone();
        if let Some((id, index)) = self.confirmed
            && let Some(profile) = library.profiles.iter_mut().find(|p| p.id == id)
        {
            profile.volume = index;
        }
        library
    }
}

#[derive(Clone)]
pub struct VolumeView {
    pub library: Library,
    pub draft: Option<Profile>,
    pub editing: bool,
    pub message: String,
}

impl VolumeController {
    pub fn new(
        path: PathBuf,
        registry: ReceiverRegistry,
        transmission: AudioTransmissionGate,
        shutdown: CancellationToken,
    ) -> Self {
        let (library, error) = match Library::load(&path) {
            Ok(library) => (library, None),
            Err(error) => (Library::default(), Some(format!("{error:#}"))),
        };
        let mut state = State::new(
            library,
            error
                .as_ref()
                .map_or_else(String::new, |e| format!("{e}；原文件未修改，禁止保存")),
        );
        state.load_error = error;
        let controller = Self {
            state: Arc::new(Mutex::new(state)),
            path,
            worker: Arc::new(Mutex::new(None)),
        };
        let worker = controller.clone();
        let handle = thread::spawn(move || {
            while !shutdown.is_cancelled() {
                worker.tick(&registry);
                thread::sleep(Duration::from_millis(100));
            }
            registry.deny_all();
            registry.broadcast(ControlMessage::CalibrationSession {
                request_id: 0,
                action: audio_stream_protocol::CalibrationAction::Cancel,
            });
            registry.broadcast(ControlMessage::CalibrationTone {
                request_id: 0,
                enabled: false,
            });
            transmission.set_calibrating(false);
        });
        *controller.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        controller
    }

    pub fn finish(&self) -> Result<()> {
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("音量控制线程异常终止"))?;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.dirty {
            state.ensure_writable()?;
            state
                .persisted()
                .save(&self.path)
                .context("退出时无法保存已确认音量")?;
            state.dirty = false;
        }
        Ok(())
    }

    pub fn view(&self) -> VolumeView {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        VolumeView {
            library: state.library.clone(),
            draft: state.draft.clone(),
            editing: state.editing,
            message: state.message.clone(),
        }
    }

    pub fn open(&self, registry: &ReceiverRegistry, gate: &AudioTransmissionGate) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.editing, "校准窗口已打开");
        ensure!(state.pending.is_empty(), "请等待音量指令确认后打开校准窗口");
        state.editing = true;
        state.draft = state.library.current().cloned();
        state.originals = registry
            .snapshots()
            .into_iter()
            .filter_map(|r| r.device.map(|d| (r.id, d)))
            .collect();
        state.blocked.clear();
        registry.deny_all();
        gate.set_calibrating(true);
        Ok(())
    }

    pub fn close(&self, registry: &ReceiverRegistry, gate: &AudioTransmissionGate) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        registry.broadcast(ControlMessage::CalibrationTone {
            request_id: 0,
            enabled: false,
        });
        registry.broadcast(ControlMessage::CalibrationSession {
            request_id: 0,
            action: audio_stream_protocol::CalibrationAction::Cancel,
        });
        state.pending.clear();
        for (id, original) in std::mem::take(&mut state.originals) {
            match registry.restore_volume_request(id, original.volume) {
                Ok(request) => {
                    state.pending.insert(
                        id,
                        Pending {
                            request,
                            index: original.volume,
                            at: Instant::now(),
                        },
                    );
                }
                Err(error) => state.message = format!("恢复 {} 音量失败：{error}", original.name),
            }
        }
        state.editing = false;
        state.draft = None;
        state.master_seen = None;
        state.blocked.clear();
        gate.set_calibrating(false);
    }

    pub fn set_master(&self, master: &DeviceStatus, registry: &ReceiverRegistry) -> Result<()> {
        ensure!(
            !master.fixed
                && !master.failed
                && master.speaker
                && !master.device_id.is_empty()
                && master.minimum < master.maximum,
            "请选择可调整音量的扬声器设备"
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "请先打开校准窗口");
        let old = state.draft.take();
        let same_master = old.as_ref().is_some_and(|p| p.master.matches(master));
        let mut draft = if same_master {
            old.unwrap()
        } else {
            Profile {
                id: 0,
                name: "新配置".into(),
                master: DeviceRange::from_status(master),
                followers: Vec::new(),
                volume: master.volume,
            }
        };
        for receiver in registry.snapshots() {
            let Some(device) = receiver.device else {
                continue;
            };
            if device.device_id == draft.master.id
                || device.device_id.is_empty()
                || !device.speaker
                || device.fixed
                || device.failed
                || device.minimum >= device.maximum
            {
                continue;
            }
            if let Some(follower) = draft
                .followers
                .iter_mut()
                .find(|f| f.device.id == device.device_id)
            {
                if !follower.device.matches(&device) {
                    follower.device = DeviceRange::from_status(&device);
                    follower.mapping.clear();
                }
            } else {
                draft.followers.push(Follower {
                    device: DeviceRange::from_status(&device),
                    mapping: BTreeMap::new(),
                });
            }
        }
        state.draft = Some(draft);
        Ok(())
    }

    pub fn remember(&self, id: u64, device: &DeviceStatus) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.editing && !state.originals.iter().any(|(old, _)| *old == id) {
            state.originals.push((id, device.clone()));
        }
    }

    pub fn new_draft(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "请先打开校准窗口");
        state.draft = None;
        Ok(())
    }

    pub fn edit(&self, change: impl FnOnce(&mut Profile) -> Result<()>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "仅能在校准窗口编辑");
        change(state.draft.as_mut().context("请先指定主设备")?)
    }

    pub fn set_draft(&self, draft: Profile) -> Result<()> {
        draft.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "校准窗口已关闭");
        state.draft = Some(draft);
        Ok(())
    }

    pub fn save_draft(&self, name: String, copy: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "请先打开校准窗口");
        state.ensure_writable()?;
        let mut draft = state.draft.clone().context("请先指定主设备")?;
        draft.name = name.trim().to_owned();
        if copy {
            draft.id = 0;
        }
        ensure!(
            draft.followers.iter().any(|f| !f.mapping.is_empty()),
            "请至少保存一个从设备的对应档位"
        );
        let mut library = state.persisted();
        let participating = draft
            .followers
            .iter()
            .filter(|f| !f.mapping.is_empty())
            .map(|f| f.device.id.as_str())
            .collect::<Vec<_>>();
        draft.volume = draft
            .nearest(draft.volume, &participating)
            .context("从设备没有共同校准档位，请先补全对应表")?;
        let id = library.put(draft.clone())?;
        library.save(&self.path)?;
        draft.id = id;
        state.library = library;
        state.confirmed = Some((draft.id, draft.volume));
        state.draft = Some(draft);
        state.dirty = false;
        state.master_seen = None;
        state.message.clear();
        Ok(())
    }

    pub fn select(&self, id: u64, registry: &ReceiverRegistry) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.pending.is_empty(), "请等待主从音量指令确认后切换配置");
        ensure!(
            state.library.profiles.iter().any(|p| p.id == id),
            "配置不存在"
        );
        if state.editing {
            state.draft = state.library.profiles.iter().find(|p| p.id == id).cloned();
            return Ok(());
        }
        state.ensure_writable()?;
        let mut library = state.persisted();
        library.active = Some(id);
        library.save(&self.path)?;
        registry.deny_all();
        state.confirmed = library.current().map(|p| (p.id, p.volume));
        state.library = library;
        state.pending.clear();
        state.blocked.clear();
        state.master_seen = None;
        state.needs_fence = true;
        state.dirty = false;
        state.message.clear();
        Ok(())
    }

    pub fn delete(&self, id: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(state.editing, "仅能在校准窗口删除配置");
        state.ensure_writable()?;
        let mut library = state.persisted();
        library.profiles.retain(|p| p.id != id);
        if library.active == Some(id) {
            library.active = None;
        }
        library.save(&self.path)?;
        state.confirmed = library.current().map(|p| (p.id, p.volume));
        state.library = library;
        state.draft = state.library.current().cloned();
        state.dirty = false;
        Ok(())
    }

    pub fn set_volume(&self, registry: &ReceiverRegistry, requested: u16) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(!state.editing, "校准窗口打开时不能改变正式音量");
        let snapshots = registry.snapshots();
        let active = state.library.active;
        let profile = state
            .library
            .profiles
            .iter_mut()
            .find(|p| Some(p.id) == active)
            .context("请先创建并应用校准配置")?;
        ensure!(
            snapshots
                .iter()
                .filter(|r| r.device.as_ref().is_some_and(|d| profile.master.matches(d)))
                .count()
                == 1,
            "主设备未连接、标识重复或音量范围已变化"
        );
        let devices = snapshots
            .iter()
            .filter_map(|r| r.device.as_ref())
            .collect::<Vec<_>>();
        let participating = profile.participating(&devices);
        let index = profile
            .nearest(requested, &participating)
            .context("当前设备没有共同校准档位")?;
        let master_id = snapshots
            .iter()
            .find(|r| r.device.as_ref().is_some_and(|d| profile.master.matches(d)))
            .unwrap()
            .id;
        let request = registry.panel_volume_request(master_id, index)?;
        profile.volume = index;
        state.needs_fence = false;
        state.pending.insert(
            master_id,
            Pending {
                request,
                index,
                at: Instant::now(),
            },
        );
        state.message.clear();
        state.blocked.clear();
        state.dirty = true;
        state.changed = Instant::now();
        registry.deny_all();
        Ok(())
    }

    fn tick(&self, registry: &ReceiverRegistry) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.editing {
            registry.deny_all();
            return;
        }
        let snapshots = registry.snapshots();
        let ids: BTreeSet<_> = snapshots.iter().map(|r| r.id).collect();
        state.blocked.retain(|id| ids.contains(id));
        let mut failures = Vec::new();
        state.pending.retain(|id, pending| {
            if !ids.contains(id) {
                return false;
            }
            if let Some(reply) = registry.take_reply(*id, pending.request) {
                if reply.failed || reply.volume != pending.index || !reply.speaker || reply.fixed {
                    failures.push((
                        *id,
                        format!(
                            "{} 无法应用校准音量，请检查系统限制后重新应用配置",
                            reply.name
                        ),
                    ));
                }
                return false;
            }
            if pending.at.elapsed() >= Duration::from_secs(4) {
                failures.push((*id, "音量指令未收到确认，请检查连接后重新应用配置".into()));
                return false;
            }
            true
        });
        for (id, message) in failures {
            state.blocked.insert(id);
            state.message = message;
        }
        let Some(profile) = state.library.current().cloned() else {
            registry.deny_all();
            return;
        };
        let masters: Vec<_> = snapshots
            .iter()
            .filter(|r| r.device.as_ref().is_some_and(|d| profile.master.matches(d)))
            .collect();
        if masters.len() != 1 {
            registry.deny_all();
            state.master_seen = None;
            return;
        }
        let master = masters[0];
        let device = master.device.as_ref().unwrap();
        let devices = snapshots
            .iter()
            .filter_map(|r| r.device.as_ref())
            .collect::<Vec<_>>();
        let participating = profile.participating(&devices);
        if (state.needs_fence || registry.has_panel_barrier(master.id))
            && !state.pending.contains_key(&master.id)
        {
            registry.deny_all();
            if !state.blocked.contains(&master.id)
                && let Some(index) = profile.nearest(profile.volume, &participating)
            {
                match registry.panel_volume_request(master.id, index) {
                    Ok(request) => {
                        state.master_seen = Some((master.id, device.volume));
                        state.needs_fence = false;
                        state.pending.insert(
                            master.id,
                            Pending {
                                request,
                                index,
                                at: Instant::now(),
                            },
                        );
                    }
                    Err(error) => {
                        state.message = error.to_string();
                        state.blocked.insert(master.id);
                    }
                }
            }
            return;
        }
        let mut requested = profile.volume;
        let unsolicited = registry.take_unsolicited_volume(master.id);
        if state.master_seen.is_some_and(|(id, _)| id == master.id)
            && let Some(index) = unsolicited
        {
            requested = index;
        }
        state.master_seen = Some((master.id, device.volume));
        let Some(index) = profile.nearest(requested, &participating) else {
            registry.deny_all();
            return;
        };
        if state.library.current().is_some_and(|p| p.volume != index) {
            if let Some(current) = state
                .library
                .profiles
                .iter_mut()
                .find(|p| p.id == profile.id)
            {
                current.volume = index;
            }
            state.dirty = true;
            state.changed = Instant::now();
        }
        let mut targets = vec![(master.id, index)];
        for follower in &profile.followers {
            if !participating.contains(&follower.device.id.as_str()) {
                continue;
            }
            if let Some(receiver) = snapshots.iter().find(|r| {
                r.device
                    .as_ref()
                    .is_some_and(|d| follower.device.matches(d))
            }) {
                targets.push((receiver.id, follower.mapping[&index]));
            }
        }
        let mut ready = true;
        for (id, target) in &targets {
            let actual = snapshots
                .iter()
                .find(|r| r.id == *id)
                .and_then(|r| r.device.as_ref())
                .unwrap();
            if actual.volume != *target
                || state.pending.contains_key(id)
                || state.blocked.contains(id)
            {
                ready = false;
                if !state.pending.contains_key(id) && !state.blocked.contains(id) {
                    match registry.restore_volume_request(*id, *target) {
                        Ok(request) => {
                            state.pending.insert(
                                *id,
                                Pending {
                                    request,
                                    index: *target,
                                    at: Instant::now(),
                                },
                            );
                        }
                        Err(error) => {
                            state.message = error.to_string();
                            state.blocked.insert(*id);
                        }
                    }
                }
            }
        }
        if ready {
            let expected = targets
                .iter()
                .filter_map(|(id, _)| {
                    snapshots
                        .iter()
                        .find(|r| r.id == *id)
                        .and_then(|r| r.device.clone())
                        .map(|device| (*id, device))
                })
                .collect::<Vec<_>>();
            registry.approve(&expected);
        } else {
            registry.deny_all();
        }
        if ready {
            state.confirmed = Some((profile.id, index));
        }
        if ready && state.dirty && state.changed.elapsed() >= Duration::from_millis(500) {
            match state
                .ensure_writable()
                .and_then(|()| state.persisted().save(&self.path))
            {
                Ok(()) => {
                    state.dirty = false;
                }
                Err(error) => {
                    state.message = format!("音量已应用，但无法保存：{error:#}");
                    state.changed = Instant::now() + Duration::from_secs(4);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_barrier_discards_delayed_old_event_but_accepts_new_physical_change() {
        let registry = ReceiverRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), tx);
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
        let mut library = Library::default();
        library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: PathBuf::new(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), device.clone());
        controller.tick(&registry);
        controller.set_volume(&registry, 7).unwrap();
        controller.tick(&registry);
        let ControlMessage::SetVolume { request_id, .. } = rx.try_recv().unwrap() else {
            panic!("expected volume request")
        };
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 0,
                volume: 4,
                ..device.clone()
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 7);
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id,
                volume: 7,
                ..device.clone()
            },
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 0,
                volume: 5,
                ..device.clone()
            },
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 998,
                volume: 5,
                ..device.clone()
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 5);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn failed_volume_command_does_not_create_physical_input() {
        let registry = ReceiverRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), tx);
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
        let mut library = Library::default();
        library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: PathBuf::new(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), device.clone());
        controller.tick(&registry);
        controller.set_volume(&registry, 7).unwrap();
        let ControlMessage::SetVolume { request_id, .. } = rx.try_recv().unwrap() else {
            panic!("expected volume request")
        };
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id,
                volume: 6,
                failed: true,
                ..device.clone()
            },
        );
        controller.tick(&registry);
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .blocked
                .contains(&master.id())
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 998,
                volume: 6,
                ..device.clone()
            },
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                volume: 6,
                ..device.clone()
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 7);
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .blocked
                .contains(&master.id())
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                volume: 5,
                ..device
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 5);
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .blocked
                .contains(&master.id())
        );
    }

    #[test]
    fn lost_panel_receipt_is_recovered_by_a_new_ordered_barrier() {
        let path =
            std::env::temp_dir().join(format!("soundwave-barrier-recovery-{}", std::process::id()));
        let registry = ReceiverRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), tx);
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
        let mut library = Library::default();
        let profile_id = library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: path.clone(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), device.clone());
        controller.tick(&registry);
        controller.set_volume(&registry, 7).unwrap();
        let ControlMessage::SetVolume {
            request_id: old, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected first request")
        };
        controller
            .state
            .lock()
            .unwrap()
            .pending
            .get_mut(&master.id())
            .unwrap()
            .at = Instant::now() - Duration::from_secs(5);
        controller.tick(&registry);
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .blocked
                .contains(&master.id())
        );
        assert!(registry.has_panel_barrier(master.id()));
        controller.select(profile_id, &registry).unwrap();
        controller.tick(&registry);
        let ControlMessage::SetVolume {
            request_id: fence,
            index,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected recovery request")
        };
        assert_ne!(old, fence);
        assert_eq!(index, 3);
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: old,
                volume: 7,
                ..device.clone()
            },
        );
        assert!(registry.has_panel_barrier(master.id()));
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 0,
                volume: 8,
                ..device.clone()
            },
        );
        assert!(registry.take_unsolicited_volume(master.id()).is_none());
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: fence,
                volume: 3,
                ..device.clone()
            },
        );
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: 0,
                volume: 5,
                ..device
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 5);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn selecting_profile_while_editing_keeps_active_profile() {
        let registry = ReceiverRegistry::default();
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
        let mut library = Library::default();
        let first = library
            .put(Profile {
                id: 0,
                name: "A".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let second = library
            .put(Profile {
                id: 0,
                name: "B".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 7,
            })
            .unwrap();
        library.active = Some(first);
        let mut state = State::new(library, String::new());
        state.editing = true;
        let controller = VolumeController {
            state: Arc::new(Mutex::new(state)),
            path: PathBuf::new(),
            worker: Arc::new(Mutex::new(None)),
        };
        controller.select(second, &registry).unwrap();
        assert_eq!(controller.view().library.active, Some(first));
        assert_eq!(controller.view().draft.unwrap().id, second);
    }

    #[test]
    fn activating_profile_revokes_playback_before_returning() {
        let path =
            std::env::temp_dir().join(format!("soundwave-profile-switch-{}", std::process::id()));
        let registry = ReceiverRegistry::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), tx);
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
        let mut library = Library::default();
        let first = library
            .put(Profile {
                id: 0,
                name: "A".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let second = library
            .put(Profile {
                id: 0,
                name: "B".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 7,
            })
            .unwrap();
        library.active = Some(first);
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: path.clone(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), device.clone());
        registry.approve(&[(master.id(), device)]);
        assert!(
            registry
                .playback_gate(master.id())
                .load(std::sync::atomic::Ordering::Acquire)
        );
        controller.select(second, &registry).unwrap();
        assert!(
            !registry
                .playback_gate(master.id())
                .load(std::sync::atomic::Ordering::Acquire)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unreadable_library_cannot_be_overwritten_by_a_draft() {
        let path =
            std::env::temp_dir().join(format!("soundwave-corrupt-volume-{}", std::process::id()));
        std::fs::write(&path, b"invalid profile data").unwrap();
        let error = Library::load(&path).err().unwrap().to_string();
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
        let mut state = State::new(Library::default(), String::new());
        state.load_error = Some(error);
        state.editing = true;
        state.draft = Some(Profile {
            id: 0,
            name: "Room".into(),
            master: DeviceRange::from_status(&device),
            followers: vec![Follower {
                device: DeviceRange {
                    id: "follower".into(),
                    ..DeviceRange::from_status(&device)
                },
                mapping: BTreeMap::from([(3, 3)]),
            }],
            volume: 3,
        });
        let controller = VolumeController {
            state: Arc::new(Mutex::new(state)),
            path: path.clone(),
            worker: Arc::new(Mutex::new(None)),
        };
        assert!(controller.save_draft("Room".into(), false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid profile data");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn shutdown_flushes_last_confirmed_not_pending_volume() {
        let path =
            std::env::temp_dir().join(format!("soundwave-confirmed-volume-{}", std::process::id()));
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
        let mut library = Library::default();
        library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&device),
                followers: vec![],
                volume: 3,
            })
            .unwrap();
        let mut state = State::new(library, String::new());
        state.library.profiles[0].volume = 7;
        state.dirty = true;
        let controller = VolumeController {
            state: Arc::new(Mutex::new(state)),
            path: path.clone(),
            worker: Arc::new(Mutex::new(None)),
        };
        controller.finish().unwrap();
        assert_eq!(Library::load(&path).unwrap().current().unwrap().volume, 3);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn follower_hardware_volume_is_restored_before_playback() {
        let registry = ReceiverRegistry::default();
        let (master_tx, _master_rx) = tokio::sync::mpsc::channel(16);
        let (slave_tx, mut slave_rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), master_tx);
        let slave = registry.register("127.0.0.1:1002".parse().unwrap(), slave_tx);
        let master_device = DeviceStatus {
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
        let slave_device = DeviceStatus {
            device_id: "slave".into(),
            name: "Slave".into(),
            volume: 5,
            maximum: 30,
            ..master_device.clone()
        };
        let mut library = Library::default();
        library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&master_device),
                followers: vec![Follower {
                    device: DeviceRange::from_status(&slave_device),
                    mapping: BTreeMap::from([(3, 5)]),
                }],
                volume: 3,
            })
            .unwrap();
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: PathBuf::new(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), master_device);
        registry.update_device(slave.id(), slave_device.clone());
        controller.tick(&registry);
        assert!(
            registry
                .playback_gate(slave.id())
                .load(std::sync::atomic::Ordering::Acquire)
        );
        registry.update_device(
            slave.id(),
            DeviceStatus {
                volume: 9,
                ..slave_device
            },
        );
        controller.tick(&registry);
        assert!(
            !registry
                .playback_gate(slave.id())
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(matches!(
            slave_rx.try_recv().unwrap(),
            ControlMessage::SetVolume { index: 5, .. }
        ));
    }

    #[test]
    fn old_volume_receipts_cannot_override_new_master_target() {
        let registry = ReceiverRegistry::default();
        let (master_tx, mut master_rx) = tokio::sync::mpsc::channel(16);
        let (slave_tx, mut slave_rx) = tokio::sync::mpsc::channel(16);
        let master = registry.register("127.0.0.1:1001".parse().unwrap(), master_tx);
        let slave = registry.register("127.0.0.1:1002".parse().unwrap(), slave_tx);
        let master_device = DeviceStatus {
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
        let slave_device = DeviceStatus {
            device_id: "slave".into(),
            name: "Slave".into(),
            volume: 5,
            maximum: 30,
            ..master_device.clone()
        };
        let mut library = Library::default();
        library
            .put(Profile {
                id: 0,
                name: "Room".into(),
                master: DeviceRange::from_status(&master_device),
                followers: vec![Follower {
                    device: DeviceRange::from_status(&slave_device),
                    mapping: BTreeMap::from([(3, 5), (7, 11)]),
                }],
                volume: 3,
            })
            .unwrap();
        let controller = VolumeController {
            state: Arc::new(Mutex::new(State::new(library, String::new()))),
            path: PathBuf::new(),
            worker: Arc::new(Mutex::new(None)),
        };
        registry.update_device(master.id(), master_device.clone());
        registry.update_device(slave.id(), slave_device.clone());
        controller.tick(&registry);
        controller.set_volume(&registry, 7).unwrap();
        controller.tick(&registry);
        let ControlMessage::SetVolume {
            request_id: master_request,
            ..
        } = master_rx.try_recv().unwrap()
        else {
            panic!("expected master volume request")
        };
        let ControlMessage::SetVolume {
            request_id: slave_request,
            ..
        } = slave_rx.try_recv().unwrap()
        else {
            panic!("expected follower volume request")
        };
        controller.set_volume(&registry, 3).unwrap();
        let ControlMessage::SetVolume {
            request_id: newest_request,
            index: 3,
            ..
        } = master_rx.try_recv().unwrap()
        else {
            panic!("expected newest master volume request")
        };
        registry.update_device(
            master.id(),
            DeviceStatus {
                request_id: master_request,
                volume: 7,
                ..master_device
            },
        );
        registry.update_device(
            slave.id(),
            DeviceStatus {
                request_id: slave_request,
                volume: 11,
                ..slave_device
            },
        );
        controller.tick(&registry);
        assert_eq!(controller.view().library.current().unwrap().volume, 3);
        assert!(master_rx.try_recv().is_err());
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .pending
                .get(&master.id())
                .is_some_and(|p| p.request == newest_request)
        );
        assert!(matches!(
            slave_rx.try_recv().unwrap(),
            ControlMessage::SetVolume { index: 5, .. }
        ));
    }
}
