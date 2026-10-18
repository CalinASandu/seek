//! The service's side of a sync session: serves own devices from their journals and keeps copies of
//! linked PCs' devices as ordinary (remote) devices.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use seek_core::{
    decode_index, encode_index_level, Change, Device, DeviceId, DeviceState, HostId, IndexHeader,
};
use seek_ipc::Event;
use seek_sync::pairing::MemberInfo;
use seek_sync::peers::{Peer, PeerState};
use seek_sync::{Cursor, Outgoing, Since, SyncHost};

use crate::state::{now_ms, State};

pub struct ServiceSync(pub Arc<State>);

impl State {
    /// Own devices shared with linked PCs.
    pub fn shared_devices(&self) -> Vec<Device> {
        let (sharing, unshared) = {
            let p = self.peers.lock().unwrap();
            (p.file.sharing, p.file.unshared.clone())
        };
        if !sharing {
            return Vec::new();
        }
        let indexes = self.indexes.read().unwrap();
        let reg = self.registry.read().unwrap();
        reg.devices
            .iter()
            .filter(|d| {
                d.indexed_by_host == reg.host_id
                    && !unshared.contains(&d.id)
                    && indexes.contains_key(&d.id)
            })
            .cloned()
            .collect()
    }

    fn is_shared(&self, id: DeviceId) -> bool {
        self.shared_devices().iter().any(|d| d.id == id)
    }

    /// Drops a copy of another PC's device: record, index, cursor and file.
    pub fn remove_remote_device(&self, id: DeviceId) {
        self.registry.write().unwrap().remove(id);
        self.indexes.write().unwrap().remove(&id);
        self.remote_cursors.lock().unwrap().remove(&id);
        self.dirty.lock().unwrap().remove(&id);
        let _ = std::fs::remove_file(self.index_path(id));
    }

    /// Forgets a linked PC and everything copied from it.
    pub fn drop_peer(&self, host: HostId) {
        let ids: Vec<DeviceId> = self
            .registry
            .read()
            .unwrap()
            .devices
            .iter()
            .filter(|d| d.indexed_by_host == host)
            .map(|d| d.id)
            .collect();
        for id in ids {
            self.remove_remote_device(id);
        }
        {
            let mut peers = self.peers.lock().unwrap();
            peers.remove(host);
            if let Err(e) = peers.save() {
                tracing::error!("saving linked PCs failed: {e}");
            }
        }
        self.connected.lock().unwrap().remove(&host);
        self.save_registry();
        self.emit(Event::DevicesChanged);
        self.peers_changed();
        self.invalidate();
    }

    /// Marks every device copied from `host` online or offline (session started/ended).
    pub fn set_peer_devices_online(&self, host: HostId, online: bool) {
        let mut changed = false;
        {
            let mut reg = self.registry.write().unwrap();
            for d in reg.devices.iter_mut().filter(|d| d.indexed_by_host == host) {
                let want = if online {
                    d.state
                } else {
                    DeviceState::Offline
                };
                if d.state != want {
                    d.state = want;
                    changed = true;
                }
            }
        }
        if changed {
            self.save_registry();
            self.emit(Event::DevicesChanged);
            self.invalidate();
        }
    }
}

impl SyncHost for ServiceSync {
    fn host_id(&self) -> uuid::Uuid {
        self.0.host_id()
    }

    fn name(&self) -> String {
        self.0.pc_name()
    }

    fn shared_devices(&self) -> Vec<Device> {
        self.0.shared_devices()
    }

    fn profiles(&self) -> Vec<(String, String)> {
        self.0.source.profiles()
    }

    fn outgoing(&self, device: uuid::Uuid, have: Option<Cursor>) -> Option<Outgoing> {
        let st = &self.0;
        if !st.is_shared(device) {
            return None;
        }
        if let Some(c) = have {
            // since and cursor under one journal lock: `to` must describe exactly these changes
            match st.with_journal(device, |j| (j.since(&c), j.cursor())) {
                (Since::UpToDate, _) => return None,
                (Since::Delta(changes), to) => {
                    return Some(Outgoing::Delta {
                        from: c,
                        to,
                        changes,
                    })
                }
                (Since::Full, _) => {}
            }
        }
        let (cursor, mut ix) = st.own_snapshot(device)?;
        let header = IndexHeader {
            format_version: 0,
            device_id: device,
            host_id: st.host_id(),
            checkpoint: None,
            saved_at: now_ms(),
            rows: 0,
            sync_cursor: Some((cursor.generation, cursor.seq)),
        };
        match encode_index_level(&header, &mut ix, 1) {
            Ok(bytes) => Some(Outgoing::Full { cursor, bytes }),
            Err(e) => {
                tracing::error!("encoding device {device} for a linked PC failed: {e}");
                None
            }
        }
    }

    fn held(&self, owner: uuid::Uuid) -> Vec<(uuid::Uuid, Cursor)> {
        let st = &self.0;
        let reg = st.registry.read().unwrap();
        let indexes = st.indexes.read().unwrap();
        let cursors = st.remote_cursors.lock().unwrap();
        reg.devices
            .iter()
            .filter(|d| d.indexed_by_host == owner && indexes.contains_key(&d.id))
            .filter_map(|d| cursors.get(&d.id).map(|c| (d.id, *c)))
            .collect()
    }

    fn remote_devices(
        &self,
        owner: uuid::Uuid,
        devices: Vec<Device>,
        profiles: Vec<(String, String)>,
    ) {
        let st = &self.0;
        let connected = st.connected.lock().unwrap().contains(&owner);
        let held: HashSet<DeviceId> = st.indexes.read().unwrap().keys().copied().collect();
        let gone: Vec<DeviceId> = {
            let mut reg = st.registry.write().unwrap();
            for mut d in devices.iter().cloned() {
                d.indexed_by_host = owner;
                let online =
                    connected && matches!(d.state, DeviceState::Online | DeviceState::Scanning);
                d.state = if online {
                    DeviceState::Online
                } else {
                    DeviceState::Offline
                };
                match reg.get_mut(d.id) {
                    Some(existing) if existing.indexed_by_host == owner => {
                        // keep what only this PC knows: when it first saw the copy
                        d.first_seen = existing.first_seen;
                        // and what is actually searchable here until the next delta arrives
                        if held.contains(&d.id) {
                            d.entry_count = existing.entry_count;
                        }
                        *existing = d;
                    }
                    Some(_) => tracing::warn!(
                        "{owner} sent device {} that belongs to another PC; ignored",
                        d.id
                    ),
                    None => reg.add(d),
                }
            }
            reg.devices
                .iter()
                .filter(|d| d.indexed_by_host == owner && !devices.iter().any(|x| x.id == d.id))
                .map(|d| d.id)
                .collect()
        };
        for id in gone {
            st.remove_remote_device(id);
        }
        {
            let mut peers = st.peers.lock().unwrap();
            if let Some(p) = peers.get_mut(owner) {
                if p.profiles != profiles {
                    p.profiles = profiles;
                    let _ = peers.save();
                    // who may see what on that PC changed
                    st.hidden_masks.lock().unwrap().clear();
                }
            }
        }
        st.save_registry();
        st.emit(Event::DevicesChanged);
        st.invalidate();
    }

    fn apply_delta(
        &self,
        owner: uuid::Uuid,
        device: uuid::Uuid,
        from: Cursor,
        to: Cursor,
        changes: Vec<Change>,
    ) -> Result<(), String> {
        let st = &self.0;
        match st.device(device) {
            Some(d) if d.indexed_by_host == owner => {}
            _ => return Err("unknown device".into()),
        }
        {
            let mut cursors = st.remote_cursors.lock().unwrap();
            if cursors.get(&device) != Some(&from) {
                return Err("cursor mismatch".into());
            }
            cursors.insert(device, to);
        }
        let count = st
            .apply_changes(device, &changes, None)
            .ok_or("no copy of this device")?;
        st.mark_dirty(device);
        if let Some(d) = st.registry.write().unwrap().get_mut(device) {
            d.entry_count = count;
        }
        st.invalidate();
        Ok(())
    }

    fn apply_full(
        &self,
        owner: uuid::Uuid,
        device: uuid::Uuid,
        cursor: Cursor,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let st = &self.0;
        match st.device(device) {
            Some(d) if d.indexed_by_host == owner => {}
            _ => return Err("unknown device".into()),
        }
        let (hdr, ix) = decode_index(&bytes).map_err(|e| e.to_string())?;
        if hdr.device_id != device {
            return Err("index belongs to another device".into());
        }
        let count = ix.live_count() as u64;
        st.indexes
            .write()
            .unwrap()
            .insert(device, Arc::new(RwLock::new(ix)));
        st.remote_cursors.lock().unwrap().insert(device, cursor);
        if let Some(d) = st.registry.write().unwrap().get_mut(device) {
            d.entry_count = count;
        }
        st.mark_dirty(device);
        st.save_index(device);
        st.emit(Event::DevicesChanged);
        st.invalidate();
        Ok(())
    }

    fn synced(&self, peer: uuid::Uuid) {
        let mut peers = self.0.peers.lock().unwrap();
        if let Some(p) = peers.get_mut(peer) {
            p.last_sync_ms = now_ms();
        }
        drop(peers);
        self.0.peers_changed();
    }

    fn member_joined(&self, m: MemberInfo) {
        let st = &self.0;
        if m.host_id == st.host_id() {
            return;
        }
        let mut peers = st.peers.lock().unwrap();
        if peers.get(m.host_id).is_none() {
            tracing::info!("{} joined the group", m.name);
            peers.upsert(Peer {
                host_id: m.host_id,
                name: m.name,
                public_key: m.public_key,
                addr: m.addr,
                linked_users: Vec::new(),
                profiles: Vec::new(),
                last_sync_ms: 0,
                state: PeerState::Linked,
            });
            let _ = peers.save();
        }
        drop(peers);
        st.peers_changed();
        if let Some(rt) = st.peers_rt.get() {
            rt.kick();
        }
    }

    fn unlinked_by(&self, peer: uuid::Uuid) {
        tracing::info!("linked PC {peer} removed the link");
        self.0.drop_peer(peer);
    }
}

#[cfg(test)]
mod tests;
