//! Persistent list of known devices (`devices.json`), including offline ones.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Device, DeviceId, DeviceIdentity, HostId};
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
