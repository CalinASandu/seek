//! Network side of sync: TCP listener and dialer, mDNS discovery, pairing, group joins and unlinking.
//!
//! Every connection starts with one byte from the dialer: `b'P'` = pairing, `b'N'` = sync session.
//! A pairing connection then gets a status byte from the host (`1` ok, `0` group full, `2` no code) before
//! SPAKE2 runs. Between two linked PCs the one with the lower host id dials; the other dials too once it has
//! not been connected for `FALLBACK_DIAL` (one-sided reachability, e.g. a firewall on one PC). A duplicate
//! session is dropped.
//!
//! A PC linked to a hub (see `link_hub`) syncs only through it: the listener, mDNS and direct dialing stop, and
//! one dialer loop keeps a session with the hub instead.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use seek_core::{DeviceId, HostId};
use seek_sync::noise::{self, Keys};
use seek_sync::pairing::{pair_guest, pair_host_then, MemberInfo, PairInfo, PairingCode};
use seek_sync::peers::{key_from_b64, key_to_b64, load_or_create_keys, Peer, PeerState, MAX_PCS};
use seek_sync::{run_session, Msg, Role, SessionCtl};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::hub::HubLive;
use crate::state::{now_ms, State, SyncConfig};
use crate::sync_host::ServiceSync;

pub(crate) const PAIRING: u8 = b'P';
pub(crate) const SESSION: u8 = b'N';
const STATUS_FULL: u8 = 0;
pub(crate) const STATUS_OK: u8 = 1;
const STATUS_NO_CODE: u8 = 2;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the higher host id waits for the lower one to dial before dialing itself.
const FALLBACK_DIAL: Duration = Duration::from_secs(10);
pub const MDNS_SERVICE: &str = "_seek._tcp.local.";

/// A Seek PC seen on the LAN (mDNS).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    pub host_id: uuid::Uuid,
    pub name: String,
    pub addr: String,
}

pub struct PeersRuntime {
    pub(crate) state: Arc<State>,
    pub(crate) cfg: SyncConfig,
    pub(crate) keys: Keys,
    /// Where the LAN listener is bound (the configured address while there is none).
    addr: Mutex<SocketAddr>,
    pub(crate) sessions: Mutex<HashMap<HostId, Arc<SessionCtl>>>,
    dialing: Mutex<HashSet<HostId>>,
    /// When each peer was last connected (or first known); the higher id dials after `FALLBACK_DIAL`.
    last_heard: Mutex<HashMap<HostId, Instant>>,
    /// Last dial error per peer, so a repeated failure is logged once.
    dial_errors: Mutex<HashMap<HostId, String>>,
    /// The active code and the SID of the user who asked for it.
    pairing: Mutex<Option<(PairingCode, String)>>,
    pub(crate) discovered: Mutex<HashMap<HostId, Discovered>>,
    kick: Notify,
    mdns: Mutex<Option<mdns_sd::ServiceDaemon>>,
    /// Listener and dialer of LAN mode.
    lan_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The hub dialer loop (hub mode).
    pub(crate) hub_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(crate) hub_live: Mutex<HubLive>,
    /// Wakes the hub dialer before its retry delay is over.
    pub(crate) hub_kick: Notify,
    /// An own index changed; the hub session turns bursts of these into one upload.
    pub(crate) index_dirty: Notify,
}

impl PeersRuntime {
    pub async fn start(state: Arc<State>, cfg: SyncConfig) -> anyhow::Result<Arc<PeersRuntime>> {
        let dir = state.cfg.data_dir.clone();
        let keys = tokio::task::spawn_blocking(move || load_or_create_keys(&dir)).await??;
        let rt = Arc::new(PeersRuntime {
            state: state.clone(),
            keys,
            addr: Mutex::new(SocketAddr::new(cfg.bind, cfg.port)),
            cfg,
            sessions: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            last_heard: Mutex::new(HashMap::new()),
            dial_errors: Mutex::new(HashMap::new()),
            pairing: Mutex::new(None),
            discovered: Mutex::new(HashMap::new()),
            kick: Notify::new(),
            mdns: Mutex::new(None),
            lan_tasks: Mutex::new(Vec::new()),
            hub_task: Mutex::new(None),
            hub_live: Mutex::new(HubLive::default()),
            hub_kick: Notify::new(),
            index_dirty: Notify::new(),
        });
        // linked to a hub: no listener, no discovery, no direct dialing
        let hub = state.peers.lock().unwrap().file.hub().is_some();
        if !hub {
            rt.start_lan().await?;
        }
        let _ = state.peers_rt.set(rt.clone());
        if hub {
            rt.start_hub();
        }
        Ok(rt)
    }

    /// LAN mode: listens for linked PCs, dials them, advertises and discovers PCs with mDNS.
    pub(crate) async fn start_lan(self: &Arc<Self>) -> anyhow::Result<()> {
        let listener = TcpListener::bind((self.cfg.bind, self.cfg.port)).await?;
        let addr = listener.local_addr()?;
        *self.addr.lock().unwrap() = addr;
        tracing::info!("sync listening on {addr}");
        let rt = self.clone();
        let accepting = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, from)) => {
                        let rt = rt.clone();
                        tokio::spawn(async move { rt.incoming(stream, from).await });
                    }
                    Err(e) => {
                        tracing::warn!("sync accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        let rt = self.clone();
        let dialer = tokio::spawn(async move {
            let mut tick = tokio::time::interval(rt.cfg.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = rt.kick.notified() => {}
                }
                rt.dial_due();
            }
        });
        self.lan_tasks.lock().unwrap().extend([accepting, dialer]);
        if self.cfg.mdns {
            if let Err(e) = self.start_mdns() {
                tracing::warn!("mDNS unavailable ({e}); linked PCs are reached at their last address");
            }
        }
        Ok(())
    }

    /// Stops the LAN listener, dialer, discovery and sessions.
    pub(crate) fn stop_lan(&self) {
        for t in self.lan_tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        for ctl in self.sessions.lock().unwrap().values() {
            ctl.stop.notify_one();
        }
        if let Some(d) = self.mdns.lock().unwrap().take() {
            let _ = d.shutdown();
        }
        self.discovered.lock().unwrap().clear();
        *self.pairing.lock().unwrap() = None;
    }

    /// Stops listening, dialing and every session.
    pub fn shutdown(&self) {
        self.stop_lan();
        if let Some(t) = self.hub_task.lock().unwrap().take() {
            t.abort();
        }
        if let Some(ctl) = self.hub_live.lock().unwrap().ctl.take() {
            ctl.stop.notify_one();
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        *self.addr.lock().unwrap()
    }

    pub(crate) fn host_id(&self) -> HostId {
        self.state.host_id()
    }

    /// Shows a new code (replacing any previous one) for the user `user_sid`.
    pub fn start_pairing(&self, user_sid: &str) -> PairingCode {
        let code = PairingCode::new(now_ms());
        *self.pairing.lock().unwrap() = Some((code.clone(), user_sid.to_string()));
        code
    }

    pub fn discovered(&self) -> Vec<Discovered> {
        let linked: HashSet<HostId> =
            self.state.peers.lock().unwrap().file.peers.iter().map(|p| p.host_id).collect();
        let mut v: Vec<Discovered> = self
            .discovered
            .lock()
            .unwrap()
            .values()
            .filter(|d| !linked.contains(&d.host_id))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Pushes own changes to every linked PC now and asks them to do the same.
    pub fn sync_now(&self) {
        for ctl in self.sessions.lock().unwrap().values() {
            ctl.sync_now.notify_one();
        }
        if let Some(ctl) = &self.hub_live.lock().unwrap().ctl {
            ctl.sync_now.notify_one();
        }
        self.kick.notify_one();
        self.hub_kick.notify_one();
    }

    /// Re-checks which linked PCs need dialing (new peer, new address).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub fn is_connected(&self, host: HostId) -> bool {
        self.sessions.lock().unwrap().contains_key(&host)
    }

    pub fn set_sharing(&self, on: bool) {
        self.edit_store(|f| f.sharing = on);
        self.sync_now();
    }

    pub fn set_device_shared(&self, d: DeviceId, shared: bool) {
        self.edit_store(|f| {
            f.unshared.retain(|x| *x != d);
            if !shared {
                f.unshared.push(d);
            }
        });
        self.sync_now();
    }

    /// Names of PCs known through a hub come from the hub's catalog (the next one would revert a rename).
    pub fn rename_peer(&self, host: HostId, name: String) -> Result<(), String> {
        let mut peers = self.state.peers.lock().unwrap();
        match peers.get_mut(host) {
            None => return Err("that device is not linked".into()),
            Some(p) if p.via_hub || p.role == Role::Hub => return Err("that PC is managed by the hub".into()),
            Some(p) => p.name = name,
        }
        if let Err(e) = peers.save() {
            tracing::error!("saving linked PCs failed: {e}");
        }
        drop(peers);
        self.state.peers_changed();
        Ok(())
    }

    fn edit_store(&self, f: impl FnOnce(&mut seek_sync::peers::PeerFile)) {
        let mut peers = self.state.peers.lock().unwrap();
        f(&mut peers.file);
        if let Err(e) = peers.save() {
            tracing::error!("saving linked PCs failed: {e}");
        }
        drop(peers);
        self.state.peers_changed();
    }

    /// Removes the link to `host` on both PCs (the other one is told if it is connected).
    pub async fn unlink(&self, host: HostId) -> Result<(), String> {
        match self.state.peers.lock().unwrap().get(host) {
            None => return Err("that device is not linked".into()),
            Some(p) if p.via_hub || p.role == Role::Hub => return Err("that PC is managed by the hub".into()),
            Some(_) => {}
        }
        // kept until they link again: if that PC is off now, it is told when it comes back
        self.state.peers.lock().unwrap().remember_unlinked(host);
        let ctl = self.sessions.lock().unwrap().get(&host).cloned();
        if let Some(ctl) = ctl {
            // the session sends it and ends
            ctl.send(Msg::Unlinked);
            for _ in 0..100 {
                if !self.is_connected(host) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let st = self.state.clone();
        tokio::task::spawn_blocking(move || st.drop_peer(host)).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn my_info(&self, user_sid: &str) -> PairInfo {
        let members = self
            .state
            .peers
            .lock()
            .unwrap()
            .file
            .peers
            .iter()
            .filter(|p| p.state == PeerState::Linked)
            .map(|p| MemberInfo {
                host_id: p.host_id,
                name: p.name.clone(),
                public_key: p.public_key.clone(),
                addr: p.addr.clone(),
            })
            .collect();
        PairInfo {
            host_id: self.host_id(),
            name: self.state.pc_name(),
            public_key: key_to_b64(&self.keys.public),
            user_sid: user_sid.to_string(),
            members,
            port: self.local_addr().port(),
            role: seek_sync::Role::Pc,
            addrs: Vec::new(),
            // the service does not track display names of users; filled in when the hub work needs it
            user_name: String::new(),
        }
    }

    fn store_peer(&self, p: Peer) {
        self.last_heard.lock().unwrap().insert(p.host_id, Instant::now());
        let mut peers = self.state.peers.lock().unwrap();
        peers.upsert(p);
        if let Err(e) = peers.save() {
            tracing::error!("saving linked PCs failed: {e}");
        }
        drop(peers);
        // user links decide what is hidden on that PC's devices
        self.state.hidden_masks.lock().unwrap().clear();
    }

    /// Links this PC with the PC at `addr` that shows `code`. Returns that PC's name.
    pub async fn join(&self, addr: &str, code: &str, user_sid: &str) -> Result<String, String> {
        if self.state.peers.lock().unwrap().file.hub().is_some() {
            return Err("this PC syncs through a hub; unlink it first".into());
        }
        let me = self.host_id();
        let mut io = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| format!("{addr} did not answer"))?
            .map_err(|e| format!("could not reach {addr}: {e}"))?;
        let _ = io.set_nodelay(true);
        io.write_all(&[PAIRING]).await.map_err(|e| e.to_string())?;
        let mut status = [0u8; 1];
        io.read_exact(&mut status).await.map_err(|e| format!("pairing failed: {e}"))?;
        match status[0] {
            STATUS_OK => {}
            STATUS_FULL => return Err(format!("that device's group is full ({MAX_PCS} devices)")),
            _ => return Err("that device is not showing a code (or it expired)".into()),
        }
        let mine = self.my_info(user_sid);
        let host = tokio::time::timeout(PAIRING_TIMEOUT, pair_guest(&mut io, code, &mine))
            .await
            .map_err(|_| "pairing timed out".to_string())?
            .map_err(|e| match e {
                seek_sync::Error::WrongCode => "wrong code".to_string(),
                e => format!("pairing failed: {e}"),
            })?;
        if host.host_id == me {
            return Err("that is this device".into());
        }
        self.store_peer(Peer {
            host_id: host.host_id,
            name: host.name.clone(),
            public_key: host.public_key.clone(),
            addr: Some(addr.to_string()),
            linked_users: vec![(user_sid.to_string(), host.user_sid.clone())],
            profiles: Vec::new(),
            last_sync_ms: 0,
            state: PeerState::Linked,
            role: Role::Pc,
            addrs: Vec::new(),
            via_hub: false,
        });
        for m in host.members.into_iter().filter(|m| m.host_id != me) {
            self.store_peer(Peer {
                host_id: m.host_id,
                name: m.name,
                public_key: m.public_key,
                addr: m.addr,
                linked_users: Vec::new(),
                profiles: Vec::new(),
                last_sync_ms: 0,
                state: PeerState::Linked,
                role: Role::Pc,
                addrs: Vec::new(),
                via_hub: false,
            });
        }
        tracing::info!("linked with {}", host.name);
        self.state.peers_changed();
        self.kick();
        Ok(host.name)
    }

    async fn incoming(self: Arc<Self>, mut io: TcpStream, from: SocketAddr) {
        let _ = io.set_nodelay(true);
        let mut kind = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(10), io.read_exact(&mut kind)).await {
            Ok(Ok(_)) => {}
            _ => return,
        }
        match kind[0] {
            PAIRING => self.pairing_host(io, from).await,
            SESSION => self.accept_session(io).await,
            _ => tracing::debug!("unknown connection kind from {from}"),
        }
    }

    async fn pairing_host(&self, mut io: TcpStream, from: SocketAddr) {
        let (status, code) = {
            let mut pairing = self.pairing.lock().unwrap();
            let members = self.state.peers.lock().unwrap().file.peers.len();
            if members + 1 >= MAX_PCS {
                (STATUS_FULL, None)
            } else {
                match pairing.as_mut() {
                    Some((c, sid)) if c.usable(now_ms()) => {
                        c.consume_attempt();
                        (STATUS_OK, Some((c.code.clone(), sid.clone())))
                    }
                    _ => (STATUS_NO_CODE, None),
                }
            }
        };
        if io.write_all(&[status]).await.is_err() {
            return;
        }
        let Some((code, creator)) = code else { return };
        let mine = self.my_info(&creator);
        // Saved before the reply goes out: the guest dials the moment it hears back. It is told it reached itself
        // by the reply, so that case is not stored.
        let mut added = None;
        let paired = tokio::time::timeout(
            PAIRING_TIMEOUT,
            pair_host_then(&mut io, &code, &mine, |guest| {
                if guest.host_id != self.host_id() {
                    let new = self.state.peers.lock().unwrap().get(guest.host_id).is_none();
                    added = new.then_some(guest.host_id);
                    self.store_peer(Peer {
                        host_id: guest.host_id,
                        name: guest.name,
                        public_key: guest.public_key,
                        addr: Some(SocketAddr::new(from.ip(), guest.port).to_string()),
                        linked_users: vec![(creator, guest.user_sid)],
                        profiles: Vec::new(),
                        last_sync_ms: 0,
                        state: PeerState::Linked,
                        role: Role::Pc,
                        addrs: Vec::new(),
                        via_hub: false,
                    });
                }
                async { Ok(()) }
            }),
        )
        .await;
        let guest = match paired {
            Ok(Ok(g)) => g,
            failed => {
                if let Ok(Err(e)) = &failed {
                    tracing::info!("pairing attempt from {from} failed: {e}");
                }
                // the guest never heard back, so it is not linked
                if let Some(host) = added {
                    let mut peers = self.state.peers.lock().unwrap();
                    peers.remove(host);
                    let _ = peers.save();
                }
                return;
            }
        };
        if guest.host_id == self.host_id() {
            return;
        }
        let addr = SocketAddr::new(from.ip(), guest.port).to_string();
        *self.pairing.lock().unwrap() = None;
        tracing::info!("linked with {}", guest.name);
        let member = MemberInfo {
            host_id: guest.host_id,
            name: guest.name,
            public_key: guest.public_key,
            addr: Some(addr),
        };
        for (host, ctl) in self.sessions.lock().unwrap().iter() {
            if *host != member.host_id {
                ctl.send(Msg::MemberJoined { member: member.clone() });
            }
        }
        self.state.peers_changed();
        self.kick();
    }

    async fn accept_session(self: Arc<Self>, io: TcpStream) {
        let (theirs, mut rd, mut wr) = match noise::accept(io, &self.keys, |_| true).await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("sync handshake failed: {e}");
                return;
            }
        };
        let known = {
            let peers = self.state.peers.lock().unwrap();
            peers.by_key(&theirs).filter(|p| p.state == PeerState::Linked).map(|p| p.host_id)
        };
        match known {
            Some(host) => self.run(host, rd, wr).await,
            None => {
                // Unknown key. If it claims to be a linked PC, that PC was reinstalled: it must link again.
                let Ok(Ok(frame)) = tokio::time::timeout(Duration::from_secs(5), rd.recv()).await else { return };
                let Ok(Msg::Hello { host_id, .. }) = frame.msg::<Msg>() else { return };
                let (relink, removed_by_us) = {
                    let mut peers = self.state.peers.lock().unwrap();
                    // linked while its Hello was on the way; that Hello is used up, so the dialer tries again
                    if peers.by_key(&theirs).is_some_and(|p| p.state == PeerState::Linked) {
                        return;
                    }
                    let removed_by_us = peers.was_unlinked(host_id);
                    let relink = match peers.get_mut(host_id) {
                        Some(p) if p.state != PeerState::NeedsRelink => {
                            tracing::warn!("{} came back with a different key; it must be linked again", p.name);
                            p.state = PeerState::NeedsRelink;
                            let _ = peers.save();
                            Some(true)
                        }
                        Some(_) => Some(false),
                        None => None,
                    };
                    (relink, removed_by_us)
                };
                match relink {
                    Some(true) => self.state.peers_changed(),
                    Some(false) => {}
                    // We do not know it yet: still saving this pairing, or it joined through another PC and we
                    // have not been told. It tries again.
                    None if !removed_by_us => {}
                    // It missed our unlink (it was offline). The dialer authenticated our key, so it can trust
                    // this; a PC only claiming that host id is told nothing it can use.
                    None => {
                        let hello = Msg::Hello {
                            protocol: seek_sync::SYNC_PROTOCOL,
                            host_id: self.host_id(),
                            name: self.state.pc_name(),
                            role: seek_sync::Role::Pc,
                        };
                        let _ = wr.send(&seek_sync::Frame::new(&hello, Vec::new())).await;
                        let _ = wr.send(&seek_sync::Frame::new(&Msg::Unlinked, Vec::new())).await;
                        // Let the dialer read it and hang up. Closing now, with its first frames (the device
                        // list) still unread here, resets the connection and the dialer loses the frame.
                        let _ = tokio::time::timeout(Duration::from_secs(2), async { while rd.recv().await.is_ok() {} })
                            .await;
                    }
                }
            }
        }
    }

    /// Dials every linked PC that has no session and is due (see module docs).
    fn dial_due(self: &Arc<Self>) {
        let me = self.host_id();
        let patience = FALLBACK_DIAL.min(self.cfg.interval * 2);
        let peers: Vec<Peer> = self
            .state
            .peers
            .lock()
            .unwrap()
            .file
            .peers
            .iter()
            .filter(|p| p.state == PeerState::Linked && p.addr.is_some())
            .cloned()
            .collect();
        for p in peers {
            if self.is_connected(p.host_id) {
                continue;
            }
            let heard = *self.last_heard.lock().unwrap().entry(p.host_id).or_insert_with(Instant::now);
            if me > p.host_id && heard.elapsed() < patience {
                // look again when the wait is over instead of at the next interval
                let rt = self.clone();
                let wait = patience - heard.elapsed();
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    rt.kick();
                });
                continue;
            }
            if !self.dialing.lock().unwrap().insert(p.host_id) {
                continue;
            }
            let rt = self.clone();
            tokio::spawn(async move {
                let host = p.host_id;
                let addr = p.addr.clone().unwrap_or_default();
                let name = p.name.clone();
                match rt.dial(p).await {
                    Err(e) => {
                        let repeat = rt.dial_errors.lock().unwrap().insert(host, e.clone()).as_ref() == Some(&e);
                        if repeat {
                            tracing::debug!("connecting to {name} at {addr} failed: {e}");
                        } else {
                            tracing::info!("connecting to {name} at {addr} failed: {e}");
                        }
                    }
                    Ok(()) => {
                        rt.dial_errors.lock().unwrap().remove(&host);
                    }
                }
                rt.dialing.lock().unwrap().remove(&host);
            });
        }
    }

    async fn dial(self: &Arc<Self>, p: Peer) -> Result<(), String> {
        let addr = p.addr.clone().ok_or("no address")?;
        let key = key_from_b64(&p.public_key).ok_or("bad stored key")?;
        let mut io = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| "timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let _ = io.set_nodelay(true);
        io.write_all(&[SESSION]).await.map_err(|e| e.to_string())?;
        let (rd, wr) = noise::connect(io, &self.keys, &key).await.map_err(|e| e.to_string())?;
        self.clone().run(p.host_id, rd, wr).await;
        Ok(())
    }

    /// Runs one session with `host` until it ends; a second session with the same PC is dropped.
    async fn run<R, W>(self: Arc<Self>, host: HostId, rd: noise::SecureReader<R>, wr: noise::SecureWriter<W>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (ctl, outbox) = SessionCtl::new();
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(&host) {
                return;
            }
            sessions.insert(host, ctl.clone());
        }
        self.state.connected.lock().unwrap().insert(host);
        self.last_heard.lock().unwrap().insert(host, Instant::now());
        self.state.peers_changed();
        tracing::info!("sync session with {host} started");

        let sync_host = Arc::new(ServiceSync(self.state.clone()));
        let r = run_session(sync_host, host, rd, wr, ctl.clone(), outbox, self.cfg.interval).await;
        if let Err(e) = &r {
            tracing::info!("sync session with {host} ended: {e}");
        }

        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.get(&host).is_some_and(|c| Arc::ptr_eq(c, &ctl)) {
                sessions.remove(&host);
            }
        }
        self.state.connected.lock().unwrap().remove(&host);
        self.last_heard.lock().unwrap().insert(host, Instant::now());
        let st = self.state.clone();
        let _ = tokio::task::spawn_blocking(move || st.set_peer_devices_online(host, false)).await;
        self.state.peers_changed();
        self.state.invalidate();
    }

    fn start_mdns(self: &Arc<Self>) -> Result<(), mdns_sd::Error> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let id = self.host_id().to_string();
        let host_name = format!("seek-{}.local.", self.host_id().simple());
        let name = self.state.pc_name();
        let props = [("id", id.as_str()), ("name", name.as_str())];
        let info = mdns_sd::ServiceInfo::new(MDNS_SERVICE, &id, &host_name, (), self.local_addr().port(), &props[..])?
            .enable_addr_auto();
        daemon.register(info)?;
        let events = daemon.browse(MDNS_SERVICE)?;
        *self.mdns.lock().unwrap() = Some(daemon);
        let rt = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("seek-mdns".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    let Some(rt) = rt.upgrade() else { break };
                    if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
                        rt.resolved(&info);
                    }
                }
            })
            .ok();
        Ok(())
    }

    fn resolved(&self, info: &mdns_sd::ServiceInfo) {
        let Some(host_id) = info.get_property_val_str("id").and_then(|s| s.parse::<uuid::Uuid>().ok()) else {
            return;
        };
        if host_id == self.host_id() {
            return;
        }
        // prefer IPv4: link-local IPv6 needs a scope id we don't track
        let Some(ip) = info.get_addresses().iter().min_by_key(|ip| ip.is_ipv6()).copied() else { return };
        let addr = SocketAddr::new(ip, info.get_port()).to_string();
        let name = info.get_property_val_str("name").unwrap_or("Seek PC").to_string();
        self.discovered.lock().unwrap().insert(host_id, Discovered { host_id, name, addr: addr.clone() });
        let moved = {
            let mut peers = self.state.peers.lock().unwrap();
            match peers.get_mut(host_id) {
                Some(p) if p.addr.as_deref() != Some(addr.as_str()) => {
                    p.addr = Some(addr);
                    let _ = peers.save();
                    true
                }
                _ => false,
            }
        };
        if moved {
            self.kick();
        }
    }
}
