//! Hub mode of the sync runtime: linking to a self-hosted hub, the dialer loop that keeps a session with it,
//! and the status the app shows. A PC linked to a hub syncs only through it (see the module docs of `peers`).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use seek_core::HostId;
use seek_ipc::{HubStatus, PcSummary, HUB_ERR_NO_WINDOW, HUB_ERR_UNTRUSTED, HUB_ERR_WRONG_CODE};
use seek_sync::noise;
use seek_sync::pairing::{pair_guest, HUB_CODE_DIGITS};
use seek_sync::peers::{key_from_b64, Peer, PeerState};
use seek_sync::transport::{dial, HubAddr, TlsConfig};
use seek_sync::{run_hub_session, Msg, Role, SessionCtl};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::peers::{PeersRuntime, PAIRING, PAIRING_TIMEOUT, SESSION, STATUS_OK};
use crate::state::now_ms;
use crate::sync_host::ServiceSync;

/// Connect (and TLS) timeout for the hub, which may be far away behind a tunnel.
const HUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// An index change is uploaded this long after the last one was noticed.
const HUB_DEBOUNCE: Duration = Duration::from_secs(2);
/// A PC the hub saw this recently counts as online (the hub closes idle sessions after 3 minutes).
const HUB_PC_ONLINE_MS: i64 = 3 * 60 * 1000;
/// The Noise handshake with the hub (after dialing) must finish within this.
const HUB_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The first retry after a failed attempt comes this soon; each further failure doubles the wait.
const HUB_FIRST_RETRY: Duration = Duration::from_secs(2);
/// This many handshakes in a row closed by the hub, with nothing else wrong, are read as "it does not know us".
const HUB_EOF_RELINK: u32 = 3;

/// Ends its task when dropped, so an aborted session leaks nothing.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the hub link is doing right now.
#[derive(Default)]
pub(crate) struct HubLive {
    pub(crate) online: bool,
    /// Why the last attempt failed, or what the hub refused or did.
    pub(crate) last_error: Option<String>,
    pub(crate) ctl: Option<Arc<SessionCtl>>,
    /// `(name, address)` of a hub that unlinked this PC; shown until acknowledged.
    pub(crate) unlinked: Option<(String, String)>,
    /// Handshakes in a row the hub just closed (restart, connection cap and rate limit look the same as an
    /// unknown key); only a streak of them is shown as "needs re-linking".
    pub(crate) eof_streak: u32,
}

/// The hub saw a PC at `last_seen_ms` recently enough for it to count as online.
pub(crate) fn recently_seen(last_seen_ms: i64) -> bool {
    last_seen_ms > 0 && now_ms() - last_seen_ms < HUB_PC_ONLINE_MS
}

impl PeersRuntime {
    pub(crate) fn start_hub(self: &Arc<Self>) {
        let rt = self.clone();
        let task = tokio::spawn(async move { rt.hub_loop().await });
        if let Some(old) = self.hub_task.lock().unwrap().replace(task) {
            old.abort();
        }
    }

    /// Links this PC to the hub at `addr` that shows `code`. Returns the hub's name. Direct links are replaced:
    /// their sessions end and the PCs are forgotten (their copies stay until the hub's catalog adopts them).
    /// They are not sent `Unlinked`: that makes the other side drop this PC's copies, and it is about to
    /// get the same drives from the hub (as deltas, on top of the copies it has).
    pub async fn link_hub(self: &Arc<Self>, addr: &str, code: &str, user_sid: &str) -> Result<String, String> {
        if self.state.peers.lock().unwrap().file.hub().is_some() {
            return Err("this PC is already linked to a hub; unlink it first".into());
        }
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        if code.len() != HUB_CODE_DIGITS {
            return Err(format!("the code has {HUB_CODE_DIGITS} digits"));
        }
        let typed = HubAddr::parse(addr)?;
        let no_code = || format!("{HUB_ERR_NO_WINDOW}the hub is not showing a code (or it expired)");
        let mut io = dial(&typed, &self.hub_tls(), HUB_CONNECT_TIMEOUT).await.map_err(|e| match e {
            seek_sync::Error::Untrusted(_) => format!("{HUB_ERR_UNTRUSTED}{}", hub_error_text(&typed, &e)),
            e => hub_error_text(&typed, &e),
        })?;
        io.write_all(&[PAIRING]).await.map_err(|e| e.to_string())?;
        let mut status = [0u8; 1];
        io.read_exact(&mut status).await.map_err(|_| no_code())?;
        if status[0] != STATUS_OK {
            return Err(no_code());
        }
        let mut mine = self.my_info(user_sid);
        mine.members.clear();
        let hub = tokio::time::timeout(PAIRING_TIMEOUT, pair_guest(&mut io, &code, &mine))
            .await
            .map_err(|_| "pairing timed out".to_string())?
            .map_err(|e| match e {
                seek_sync::Error::WrongCode => format!("{HUB_ERR_WRONG_CODE}wrong code"),
                e => format!("pairing failed: {e}"),
            })?;
        if hub.role != Role::Hub {
            return Err("that address is a PC, not a hub".into());
        }
        let mut addrs = hub.addrs.clone();
        addrs.push(typed.to_string());
        let mut seen = HashSet::new();
        addrs.retain(|a| seen.insert(a.clone()));
        let peer = Peer {
            host_id: hub.host_id,
            name: hub.name.clone(),
            public_key: hub.public_key.clone(),
            addr: Some(typed.to_string()),
            // the local user who linked is the same person on every PC of the hub
            linked_users: vec![(user_sid.to_string(), user_sid.to_string())],
            profiles: Vec::new(),
            last_sync_ms: 0,
            state: PeerState::Linked,
            role: Role::Hub,
            addrs,
            via_hub: false,
        };

        // end the sessions with the PCs linked directly
        let ctls: Vec<Arc<SessionCtl>> = self.sessions.lock().unwrap().values().cloned().collect();
        for ctl in &ctls {
            ctl.stop.notify_one();
        }
        for _ in 0..100 {
            if self.sessions.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let st = self.state.clone();
        tokio::task::spawn_blocking(move || st.enter_hub_mode(peer)).await.map_err(|e| e.to_string())?;
        self.stop_lan();
        *self.hub_live.lock().unwrap() = HubLive::default();
        self.start_hub();
        tracing::info!("linked with hub {}", hub.name);
        Ok(hub.name)
    }

    /// Unlinks from the hub (telling it if connected), forgets the PCs and copies received through it and
    /// returns to LAN mode. Without a hub it only dismisses the *hub unlinked this PC* notice.
    pub async fn unlink_hub(self: &Arc<Self>) -> Result<(), String> {
        if self.state.peers.lock().unwrap().file.hub().is_none() {
            let dismissed = self.hub_live.lock().unwrap().unlinked.take().is_some();
            self.state.peers_changed();
            return if dismissed { Ok(()) } else { Err("this PC is not linked to a hub".into()) };
        }
        let ctl = self.hub_live.lock().unwrap().ctl.clone();
        if let Some(ctl) = ctl {
            // the session sends it and ends
            ctl.send(Msg::Unlinked);
            for _ in 0..100 {
                if self.hub_live.lock().unwrap().ctl.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        if let Some(t) = self.hub_task.lock().unwrap().take() {
            t.abort();
        }
        let st = self.state.clone();
        tokio::task::spawn_blocking(move || st.leave_hub()).await.map_err(|e| e.to_string())?;
        *self.hub_live.lock().unwrap() = HubLive::default();
        self.state.peers_changed();
        self.start_lan().await.map_err(|e| format!("sync on the local network unavailable: {e}"))
    }

    /// The hub link as the app shows it; `None` when this PC is not linked to a hub.
    pub fn hub_status(&self) -> Option<HubStatus> {
        let (hub, pcs) = {
            let peers = self.state.peers.lock().unwrap();
            (peers.file.hub().cloned(), peers.file.peers.iter().filter(|p| p.via_hub).cloned().collect::<Vec<_>>())
        };
        let live = self.hub_live.lock().unwrap();
        match hub {
            Some(h) => Some(HubStatus {
                name: h.name,
                addr: h.addr.unwrap_or_default(),
                online: live.online,
                last_sync_ms: h.last_sync_ms,
                last_error: live.last_error.clone(),
                pcs: pcs
                    .into_iter()
                    .map(|p| PcSummary {
                        host_id: p.host_id,
                        online: live.online && recently_seen(p.last_sync_ms),
                        name: p.name,
                        // for a PC known through the hub: when the hub last saw it
                        last_sync_ms: p.last_sync_ms,
                        needs_relink: false,
                    })
                    .collect(),
                unlinked: false,
            }),
            None => live.unlinked.as_ref().map(|(name, addr)| HubStatus {
                name: name.clone(),
                addr: addr.clone(),
                online: false,
                last_sync_ms: 0,
                last_error: Some("The hub unlinked this PC".into()),
                pcs: Vec::new(),
                unlinked: true,
            }),
        }
    }

    /// Is this linked PC reachable now (a session with it, or seen recently by the hub)?
    pub fn peer_online(&self, p: &Peer) -> bool {
        if p.via_hub {
            self.hub_live.lock().unwrap().online && recently_seen(p.last_sync_ms)
        } else {
            self.is_connected(p.host_id)
        }
    }

    /// The hub removed the link (the service has already forgotten it): status says so until dismissed.
    pub(crate) fn hub_unlinked(&self, hub: &Peer) {
        let mut live = self.hub_live.lock().unwrap();
        live.unlinked = Some((hub.name.clone(), hub.addr.clone().unwrap_or_default()));
        live.last_error = None;
        drop(live);
        self.state.peers_changed();
    }

    /// Something the hub refused or did, for the status line.
    pub(crate) fn hub_note(&self, text: &str) {
        self.hub_live.lock().unwrap().last_error = Some(text.to_string());
        self.state.peers_changed();
    }

    /// An own index changed: the hub session uploads soon.
    pub(crate) fn index_changed(&self) {
        self.index_dirty.notify_one();
    }

    fn hub_tls(&self) -> TlsConfig {
        self.cfg.hub_tls.clone().unwrap_or_else(TlsConfig::public)
    }

    /// Keeps a session with the hub: after a failure it retries soon (2 s) and doubles the wait per failure
    /// up to twice the polling interval; after a session it reconnects after one interval. Ends (back to LAN
    /// mode) when the hub unlinks this PC.
    async fn hub_loop(self: Arc<Self>) {
        let (base, cap) = (self.cfg.interval, self.cfg.interval * 2);
        let first = HUB_FIRST_RETRY.min(base);
        let mut wait = first;
        loop {
            let Some(hub) = self.state.peers.lock().unwrap().file.hub().cloned() else { break };
            let (established, result) = self.hub_connect(&hub).await;
            if let Err(e) = result {
                tracing::info!("hub {}: {e}", hub.name);
                self.hub_live.lock().unwrap().last_error = Some(e);
                self.state.peers_changed();
            }
            if self.state.peers.lock().unwrap().file.hub().is_none() {
                break;
            }
            let delay = if established {
                wait = first;
                base
            } else {
                let d = wait;
                wait = (wait * 2).min(cap);
                d
            };
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.hub_kick.notified() => {}
            }
        }
        // The hub unlinked this PC: back to the local network. `unlink_hub` also calls `start_lan`, but it
        // aborts this loop first, so only one of the two ever runs.
        if let Err(e) = self.start_lan().await {
            tracing::error!("sync on the local network unavailable: {e}");
        }
    }

    /// One attempt: dial the hub's addresses in order, authenticate and run a session. Returns whether a
    /// session was established, and why the attempt or session failed.
    async fn hub_connect(self: &Arc<Self>, hub: &Peer) -> (bool, Result<(), String>) {
        let Some(key) = key_from_b64(&hub.public_key) else { return (false, Err("bad stored hub key".into())) };
        let tls = self.hub_tls();
        let mut last = "the hub has no address".to_string();
        let mut stream = None;
        for a in &hub.addrs {
            let addr = match HubAddr::parse(a) {
                Ok(addr) => addr,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            match dial(&addr, &tls, HUB_CONNECT_TIMEOUT).await {
                Ok(io) => {
                    stream = Some(io);
                    break;
                }
                Err(e) => last = hub_error_text(&addr, &e),
            }
        }
        let Some(mut io) = stream else { return (false, Err(last)) };
        let handshake = tokio::time::timeout(HUB_HANDSHAKE_TIMEOUT, async {
            io.write_all(&[SESSION]).await.map_err(seek_sync::Error::Io)?;
            noise::connect(io, &self.keys, &key).await
        })
        .await;
        let (rd, wr) = match handshake {
            Ok(Ok(x)) => x,
            Err(_) => return (false, Err("the hub did not answer the handshake in time".into())),
            // The hub has another key than the one we stored (reinstalled) or does not know ours.
            Ok(Err(seek_sync::Error::Crypto(_))) => return (false, Err(RELINK_TEXT.into())),
            // The hub also closes on restart, connection cap, rate limit and its 5 s deadline, so one close is
            // not proof: a streak of them is.
            Ok(Err(seek_sync::Error::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let streak = {
                    let mut live = self.hub_live.lock().unwrap();
                    live.eof_streak += 1;
                    live.eof_streak
                };
                let text = if streak >= HUB_EOF_RELINK {
                    RELINK_TEXT
                } else {
                    "The hub closed the connection (it may be restarting)"
                };
                return (false, Err(text.into()));
            }
            Ok(Err(e)) => {
                self.hub_live.lock().unwrap().eof_streak = 0;
                return (false, Err(format!("could not connect to the hub: {e}")));
            }
        };

        let (ctl, outbox) = SessionCtl::new();
        {
            let mut live = self.hub_live.lock().unwrap();
            live.online = true;
            live.last_error = None;
            live.ctl = Some(ctl.clone());
            live.eof_streak = 0;
        }
        // a kick from before this session would cut the next backoff short for nothing
        let _ = tokio::time::timeout(Duration::ZERO, self.hub_kick.notified()).await;
        self.state.peers_changed();
        tracing::info!("hub session with {} started", hub.name);
        let _debounce = {
            let (rt, ctl) = (self.clone(), ctl.clone());
            AbortOnDrop(tokio::spawn(async move {
                loop {
                    rt.index_dirty.notified().await;
                    tokio::time::sleep(HUB_DEBOUNCE).await;
                    ctl.changed.notify_one();
                }
            }))
        };
        let host = Arc::new(ServiceSync(self.state.clone()));
        let r = run_hub_session(host, hub.host_id, rd, wr, ctl, outbox, self.cfg.interval).await;
        {
            let mut live = self.hub_live.lock().unwrap();
            live.online = false;
            live.ctl = None;
        }
        // the hub is out of reach: its PCs' drives are offline until it answers again
        let st = self.state.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let via: Vec<HostId> =
                st.peers.lock().unwrap().file.peers.iter().filter(|p| p.via_hub).map(|p| p.host_id).collect();
            for host in via {
                st.connected.lock().unwrap().remove(&host);
                st.set_peer_devices_online(host, false);
            }
        })
        .await;
        self.state.peers_changed();
        self.state.invalidate();
        (true, r.map_err(|e| e.to_string()))
    }
}

const RELINK_TEXT: &str = "Hub needs re-linking. Unlink and link it again.";

fn hub_error_text(addr: &HubAddr, e: &seek_sync::Error) -> String {
    match e {
        seek_sync::Error::Untrusted(why) => format!("Hub address not trusted ({why})"),
        e => format!("could not reach {addr}: {e}"),
    }
}
