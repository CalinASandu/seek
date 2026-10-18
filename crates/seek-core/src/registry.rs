//! Persistent list of known devices (`devices.json`), including offline ones.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Device, DeviceId, DeviceIdentity, DeviceState, HostId};
use crate::persist::write_atomic;
use crate::Error;

const FILE_NAME: &str = "devices.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistry {
    pub host_id: HostId,
    pub devices: Vec<Device>,
}

impl DeviceRegistry {
    /// Load `<dir>/devices.json`; when missing, create a registry with a new random host id and persist it.
    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        let path = dir.join(FILE_NAME);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let reg = Self {
                    host_id: uuid::Uuid::new_v4(),
                    devices: Vec::new(),
                };
                reg.save(dir)?;
                Ok(reg)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(dir)?;
        write_atomic(&dir.join(FILE_NAME), &serde_json::to_vec_pretty(self)?)
    }

    pub fn find_by_identity(&self, ident: &DeviceIdentity) -> Option<&Device> {
        self.devices.iter().find(|d| &d.identity == ident)
    }

    /// Like [`Self::find_by_identity`], but only devices this PC indexes itself (not copies from linked PCs).
    pub fn find_own_by_identity(&self, ident: &DeviceIdentity) -> Option<&Device> {
        self.devices.iter().find(|d| &d.identity == ident && d.indexed_by_host == self.host_id)
    }

    pub fn get(&self, id: DeviceId) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: DeviceId) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    pub fn add(&mut self, d: Device) {
        self.devices.push(d);
    }

    pub fn remove(&mut self, id: DeviceId) -> Option<Device> {
        let pos = self.devices.iter().position(|d| d.id == id)?;
        Some(self.devices.remove(pos))
    }
}

/// Devices to hide because another record describes the same physical device (a USB stick seen by two
/// PCs): a connected local record wins, otherwise the most recently indexed one (ties: local, then id).
pub fn hidden_duplicates(devices: &[Device], local: HostId) -> HashSet<DeviceId> {
    let mut groups: HashMap<&DeviceIdentity, Vec<&Device>> = HashMap::new();
    for d in devices {
        groups.entry(&d.identity).or_default().push(d);
    }
    let rank = |d: &Device| {
        let is_local = d.indexed_by_host == local;
        let connected = is_local && matches!(d.state, DeviceState::Online | DeviceState::Scanning);
        (connected, d.index_updated_at, is_local, std::cmp::Reverse(d.id))
    };
    let mut hidden = HashSet::new();
    for group in groups.into_values().filter(|g| g.len() > 1) {
        let keep = group.iter().max_by_key(|d| rank(d)).map(|d| d.id);
        hidden.extend(group.iter().map(|d| d.id).filter(|id| Some(*id) != keep));
    }
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn dev(host: HostId, serial: u64, updated: i64, state: DeviceState) -> Device {
        Device {
            id: uuid::Uuid::new_v4(),
            identity: DeviceIdentity::Volume { serial, filesystem: "exFAT".into() },
            display_name: "Stick".into(),
            label: String::new(),
            filesystem: "exFAT".into(),
            kind: DeviceKind::Removable,
            total_bytes: 0,
            free_bytes: 0,
            last_mount_point: "E:\\".into(),
            indexed_by_host: host,
            first_seen: 0,
            last_seen: 0,
            index_updated_at: updated,
            state,
            entry_count: 0,
            error: None,
        }
    }

    #[test]
    fn own_identity_lookup_ignores_remote_copies() {
        let me = uuid::Uuid::new_v4();
        let remote = dev(uuid::Uuid::new_v4(), 7, 5, DeviceState::Offline);
        let reg = DeviceRegistry { host_id: me, devices: vec![remote.clone()] };
        assert!(reg.find_own_by_identity(&remote.identity).is_none());
        assert!(reg.find_by_identity(&remote.identity).is_some());
    }

    #[test]
    fn duplicates_newest_wins_and_connected_local_always_wins() {
        let me = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let local_old = dev(me, 7, 10, DeviceState::Offline);
        let remote_new = dev(other, 7, 20, DeviceState::Offline);
        let unrelated = dev(other, 8, 1, DeviceState::Offline);
        let hidden = hidden_duplicates(&[local_old.clone(), remote_new.clone(), unrelated.clone()], me);
        assert_eq!(hidden, [local_old.id].into_iter().collect());
        let local_online = Device { state: DeviceState::Online, ..local_old.clone() };
        let hidden = hidden_duplicates(&[local_online, remote_new.clone()], me);
        assert_eq!(hidden, [remote_new.id].into_iter().collect());
    }
}
