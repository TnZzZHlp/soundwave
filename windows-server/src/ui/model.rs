use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::application::{
    calibration::CalibrationRequest, registry::ReceiverSnapshot, state::ServerState,
};
use audio_stream_protocol::ControlMessage;

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
    Select(u64),
    Volume(u16),
    Group(u16),
    Ceiling(u16),
    Preview,
    Calibrate,
    Cancel,
    Apply,
    Exit,
    CopyPairing,
    CopyFingerprint,
}

pub(super) struct Controller {
    pub server: ServerState,
    pub receivers: Vec<ReceiverSnapshot>,
    pub selected: Option<u64>,
    pub page: Page,
    pub group: u16,
    pub ceiling: u16,
    pub notice: String,
    pending: Option<(u64, u16, Instant)>,
    group_pending: Option<Instant>,
    replies: BTreeMap<u64, (u32, Instant)>,
}

impl Controller {
    pub fn new(server: ServerState) -> Self {
        let mut controller = Self {
            server,
            receivers: Vec::new(),
            selected: None,
            page: Page::Receivers,
            group: 100,
            ceiling: 75,
            notice: String::new(),
            pending: None,
            group_pending: None,
            replies: BTreeMap::new(),
        };
        controller.refresh();
        controller
    }

    pub fn refresh(&mut self) {
        self.receivers = self.server.registry.snapshots();
        if !self.receivers.iter().any(|r| Some(r.id) == self.selected) {
            self.selected = self.receivers.first().map(|r| r.id);
        }
        let mut errors = Vec::new();
        self.replies.retain(|id, (request, at)| {
            if let Some(reply) = self.server.registry.take_reply(*id, *request) {
                if reply.failed {
                    errors.push(format!(
                        "{} 拒绝了音量调整请求，请检查系统策略或安全音量限制",
                        reply.name
                    ));
                }
                false
            } else if !self.receivers.iter().any(|r| r.id == *id) {
                false
            } else if at.elapsed() >= Duration::from_secs(4) {
                errors.push("音量调整未收到确认，请检查设备连接".into());
                false
            } else {
                true
            }
        });
        if !errors.is_empty() {
            self.notice = errors.join("; ");
        }
        if self
            .pending
            .is_some_and(|(_, _, at)| at.elapsed() >= Duration::from_millis(120))
            && let Some((id, index, _)) = self.pending.take()
            && self.server.calibration.can_adjust()
        {
            match self.server.registry.request(id, Some(index)) {
                Ok(request) => {
                    self.replies.insert(id, (request, Instant::now()));
                }
                Err(error) => self.notice = error.to_string(),
            }
        }
        if self
            .group_pending
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(120))
        {
            self.group_pending = None;
            let result = self
                .server
                .calibration
                .apply_group(&self.server.registry, self.group);
            self.track(result);
        }
    }

    pub fn selected_device(&self) -> Option<&audio_stream_protocol::DeviceStatus> {
        self.receivers
            .iter()
            .find(|r| Some(r.id) == self.selected)?
            .device
            .as_ref()
    }

    pub fn volume(&self) -> Option<u16> {
        self.pending
            .filter(|(id, _, _)| Some(*id) == self.selected)
            .map(|(_, index, _)| index)
            .or_else(|| self.selected_device().map(|d| d.volume))
    }

    pub fn command(&mut self, command: Command) {
        match command {
            Command::Show(page) => self.page = page,
            Command::Toggle if !self.server.calibration.status().0 => {
                self.server
                    .transmission
                    .set_enabled(!self.server.transmission.is_enabled());
            }
            Command::AdjustAll(steps) if self.server.calibration.can_adjust() => {
                self.server
                    .registry
                    .broadcast(ControlMessage::AdjustVolume { steps });
            }
            Command::Select(id) if !self.server.calibration.status().0 => {
                self.selected = Some(id);
            }
            Command::Volume(index) if self.server.calibration.can_adjust() => {
                if let (Some(id), Some(device)) = (self.selected, self.selected_device())
                    && !device.fixed
                {
                    self.pending = Some((
                        id,
                        index.clamp(device.minimum, device.maximum),
                        Instant::now(),
                    ));
                    self.notice.clear();
                }
            }
            Command::Group(percent) if !self.server.calibration.status().0 => {
                self.group = percent.min(100);
                self.group_pending = Some(Instant::now());
                self.notice.clear();
            }
            Command::Ceiling(percent) if !self.server.calibration.status().0 => {
                self.ceiling = percent.clamp(1, 100);
            }
            Command::Preview | Command::Calibrate => {
                if self.pending.is_some()
                    || self.group_pending.is_some()
                    || !self.replies.is_empty()
                {
                    self.notice = "请等待音量调整得到设备确认后再开始播放".into();
                    return;
                }
                let preview = matches!(command, Command::Preview);
                if let Some(reference) = self.selected {
                    let result = self.server.calibration.start(CalibrationRequest {
                        reference,
                        ceiling_percent: self.ceiling,
                        preview,
                        registry: self.server.registry.clone(),
                        transmission: self.server.transmission.clone(),
                        shutdown: self.server.shutdown.clone(),
                        profile_path: self.server.info.identity_dir.join("volume-calibration.txt"),
                    });
                    self.notice = result.err().map_or_else(String::new, |e| e.to_string());
                }
            }
            Command::Cancel => self.server.calibration.cancel(),
            Command::Apply if !self.server.calibration.status().0 => {
                if self.pending.is_some() || self.group_pending.is_some() {
                    self.notice = "请等待音量调整完成后再应用已保存的校准".into();
                    return;
                }
                self.group = 100;
                let result = self.server.calibration.apply_saved(&self.server.registry);
                self.track(result);
            }
            Command::Exit => self.server.shutdown.cancel(),
            _ => {}
        }
    }

    fn track(&mut self, result: anyhow::Result<Vec<(u64, u32)>>) {
        match result {
            Ok(requests) => {
                self.notice.clear();
                for (id, request) in requests {
                    self.replies.insert(id, (request, Instant::now()));
                }
            }
            Err(error) => self.notice = error.to_string(),
        }
    }
}
