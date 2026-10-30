use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use audio_stream_protocol::DeviceStatus;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRange {
    pub id: String,
    pub name: String,
    pub minimum: u16,
    pub maximum: u16,
}

impl DeviceRange {
    pub fn from_status(device: &DeviceStatus) -> Self {
        Self {
            id: device.device_id.clone(),
            name: device.name.clone(),
            minimum: device.minimum,
            maximum: device.maximum,
        }
    }

    pub fn matches(&self, device: &DeviceStatus) -> bool {
        self.id == device.device_id
            && self.minimum == device.minimum
            && self.maximum == device.maximum
            && device.speaker
            && !device.fixed
            && !device.failed
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Follower {
    pub device: DeviceRange,
    pub mapping: BTreeMap<u16, u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    pub master: DeviceRange,
    pub followers: Vec<Follower>,
    pub volume: u16,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty() && self.name.chars().count() <= 80,
            "配置名称须为 1–80 个字符"
        );
        let mut ids = BTreeSet::new();
        for device in std::iter::once(&self.master).chain(self.followers.iter().map(|f| &f.device))
        {
            ensure!(
                !device.id.is_empty()
                    && device.id.len() <= 80
                    && device
                        .id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "设备标识无效"
            );
            ensure!(ids.insert(&device.id), "配置中存在重复设备");
            ensure!(
                device.minimum < device.maximum && device.maximum <= 512,
                "设备音量范围无效"
            );
        }
        ensure!(
            (self.master.minimum..=self.master.maximum).contains(&self.volume),
            "主设备音量超出范围"
        );
        for follower in &self.followers {
            ensure!(
                follower
                    .mapping
                    .iter()
                    .all(
                        |(master, index)| (self.master.minimum..=self.master.maximum)
                            .contains(master)
                            && (follower.device.minimum..=follower.device.maximum).contains(index)
                    ),
                "校准映射超出设备范围"
            );
        }
        Ok(())
    }

    pub fn participating<'a>(&'a self, devices: &[&DeviceStatus]) -> Vec<&'a str> {
        self.followers
            .iter()
            .filter(|f| {
                !f.mapping.is_empty() && devices.iter().filter(|d| f.device.matches(d)).count() == 1
            })
            .map(|f| f.device.id.as_str())
            .collect()
    }

    pub fn available(&self, participating: &[&str]) -> Vec<u16> {
        (self.master.minimum..=self.master.maximum)
            .filter(|index| {
                self.followers
                    .iter()
                    .filter(|f| participating.contains(&f.device.id.as_str()))
                    .all(|f| f.mapping.contains_key(index))
            })
            .collect()
    }

    pub fn nearest(&self, requested: u16, participating: &[&str]) -> Option<u16> {
        self.available(participating)
            .into_iter()
            .min_by_key(|index| index.abs_diff(requested))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Library {
    version: u16,
    pub active: Option<u64>,
    pub profiles: Vec<Profile>,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            version: 1,
            active: None,
            profiles: Vec::new(),
        }
    }
}

impl Library {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("无法检查音量配置文件"),
        };
        ensure!(metadata.len() <= 8 * 1024 * 1024, "配置文件过大");
        let bytes = fs::read(path).context("无法读取音量配置")?;
        let (library, tail): (Self, _) =
            postcard::take_from_bytes(&bytes).context("音量配置文件损坏")?;
        ensure!(
            tail.is_empty() && library.version == 1,
            "不支持的音量配置格式"
        );
        ensure!(library.profiles.len() <= 128, "配置数量超出限制");
        let mut ids = BTreeSet::new();
        for profile in &library.profiles {
            profile.validate()?;
            ensure!(ids.insert(profile.id), "配置 ID 重复");
        }
        ensure!(
            library.active.is_none_or(|id| ids.contains(&id)),
            "活动配置不存在"
        );
        Ok(library)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = postcard::to_allocvec(self)?;
        ensure!(bytes.len() <= 8 * 1024 * 1024, "音量配置超过文件大小限制");
        let temporary = path.with_extension("tmp");
        let mut file = fs::File::create(&temporary).context("无法保存音量配置")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, path).context("无法替换音量配置文件")
    }

    pub fn current(&self) -> Option<&Profile> {
        self.profiles.iter().find(|p| Some(p.id) == self.active)
    }

    pub fn put(&mut self, mut profile: Profile) -> Result<u64> {
        profile.name = profile.name.trim().to_owned();
        profile.validate()?;
        if profile.id == 0 {
            ensure!(self.profiles.len() < 128, "最多保存 128 套配置");
            profile.id = self
                .profiles
                .iter()
                .map(|p| p.id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .context("配置 ID 耗尽")?;
        }
        let id = profile.id;
        if let Some(old) = self.profiles.iter_mut().find(|p| p.id == id) {
            *old = profile;
        } else {
            self.profiles.push(profile);
        }
        self.active = Some(id);
        Ok(id)
    }
}

fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                &windows::core::HSTRING::from(source.as_os_str()),
                &windows::core::HSTRING::from(destination.as_os_str()),
                windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                    | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participating_followers_restrict_master_to_explicit_points() {
        let p = Profile {
            id: 1,
            name: "Room".into(),
            master: DeviceRange {
                id: "master".into(),
                name: "Master".into(),
                minimum: 0,
                maximum: 15,
            },
            followers: vec![Follower {
                device: DeviceRange {
                    id: "slave".into(),
                    name: "Slave".into(),
                    minimum: 0,
                    maximum: 30,
                },
                mapping: BTreeMap::from([(2, 5), (4, 9)]),
            }],
            volume: 2,
        };
        assert_eq!(p.available(&["slave"]), vec![2, 4]);
        assert_eq!(p.nearest(3, &["slave"]), Some(2));
    }

    #[test]
    fn no_shared_mapping_disables_group() {
        let mut p = Profile {
            id: 1,
            name: "Room".into(),
            master: DeviceRange {
                id: "master".into(),
                name: "Master".into(),
                minimum: 0,
                maximum: 15,
            },
            followers: vec![Follower {
                device: DeviceRange {
                    id: "slave".into(),
                    name: "Slave".into(),
                    minimum: 0,
                    maximum: 30,
                },
                mapping: BTreeMap::from([(2, 5), (4, 9)]),
            }],
            volume: 2,
        };
        let mut other = p.followers[0].clone();
        other.device.id = "other".into();
        other.mapping = BTreeMap::from([(3, 7)]);
        p.followers.push(other);
        assert!(p.available(&["slave", "other"]).is_empty());
    }

    #[test]
    fn mapping_at_system_maximum_survives_serialization() {
        let mut p = Profile {
            id: 1,
            name: "Room".into(),
            master: DeviceRange {
                id: "master".into(),
                name: "Master".into(),
                minimum: 0,
                maximum: 15,
            },
            followers: vec![Follower {
                device: DeviceRange {
                    id: "slave".into(),
                    name: "Slave".into(),
                    minimum: 0,
                    maximum: 30,
                },
                mapping: BTreeMap::from([(2, 5), (4, 9)]),
            }],
            volume: 2,
        };
        p.followers[0].mapping.insert(15, 30);
        let mut library = Library::default();
        library.put(p).unwrap();
        let bytes = postcard::to_allocvec(&library).unwrap();
        let restored: Library = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(
            restored.current().unwrap().followers[0].mapping.get(&15),
            Some(&30)
        );
    }
}
