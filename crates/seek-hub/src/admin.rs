//! The admin endpoint (spec §8): `seek-hub <command>` talks to the running daemon through it. On unix it is
//! a socket at `<data>/admin.sock` (mode 0600, inside the container); elsewhere (dev and tests on Windows)
//! `127.0.0.1:<random port>`, with the port and a random token in `<data>/admin.port`. One JSON request per
//! line (at most [`MAX_REQUEST`] bytes), one JSON reply, then the connection closes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use uuid::Uuid;

use crate::config::HubConfig;
use crate::counters::CounterValues;
use crate::crypt::HubKey;
use crate::lock::DataLock;
use crate::net::Hub;
use crate::pairing::{self, PairingState};
use crate::rotate::{self, READ_ONLY_HINT};
use crate::session::now_ms;
use crate::store::Store;

/// Longest request line accepted (a request is a few dozen bytes).
pub const MAX_REQUEST: usize = 4 * 1024;
/// Longest reply the client reads (a status with many PCs).
const MAX_REPLY: u64 = 16 * 1024 * 1024;
/// A connection must send its request within this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The client gives up on a reply after this (the healthcheck must not hang)...
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// ...except for a key rotation, which re-encrypts every stored copy first.
const ROTATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
#[cfg(unix)]
const SOCK_FILE: &str = "admin.sock";
#[cfg(not(unix))]
const PORT_FILE: &str = "admin.port";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum AdminReq {
    Link,
    Status,
    Pcs,
    Unlink { pc: String },
    RotateKey,
}

/// A request on the wire; `token` only matters on the TCP endpoint.
#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(default)]
    token: String,
    req: AdminReq,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AdminResp {
    Link(LinkInfo),
    Status(HubStatus),
    Pcs { pcs: Vec<PcInfo> },
    Unlinked { name: String, host_id: Uuid },
    Rotated { files: usize, resumed: bool },
    Error { message: String },
}

/// A pairing window just opened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkInfo {
    pub code: String,
    pub expires_ms: i64,
    /// The window's number in [`PairingState::window`].
    pub window: u64,
    /// What the pairing reply tells the PC.
    pub addrs: Vec<String>,
    pub public: Option<String>,
    /// The hub also listens on every interface (`SEEK_HUB_LAN=1`), port [`crate::config::DEFAULT_PORT`].
    pub lan: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubStatus {
    pub name: String,
    pub hub_id: Uuid,
    /// The hub's Noise public key (base64): what a PC must reach.
    pub public_key: String,
    pub listening: Vec<String>,
    pub public: Option<String>,
    pub lan: bool,
    pub pc_count: usize,
    pub devices: usize,
    pub stored_bytes: u64,
    pub counters: CounterValues,
    pub pcs: Vec<PcInfo>,
    pub pairing: PairingState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcInfo {
    pub host_id: Uuid,
    pub name: String,
    pub user_name: String,
    pub last_seen_ms: i64,
    pub devices: usize,
    pub stored_bytes: u64,
}

fn error(message: impl Into<String>) -> AdminResp {
    AdminResp::Error { message: message.into() }
}

// ---- the daemon side ----

/// What the admin commands act on.
pub struct Admin {
    hub: Arc<Hub>,
    store: Arc<Store>,
    cfg: HubConfig,
    rotating: tokio::sync::Mutex<()>,
}

impl Admin {
    pub fn new(hub: Arc<Hub>, store: Arc<Store>, cfg: HubConfig) -> Arc<Admin> {
        Arc::new(Admin { hub, store, cfg, rotating: tokio::sync::Mutex::new(()) })
    }

    fn lan(&self) -> bool {
        self.cfg.listen.iter().any(|a| a.ip().is_unspecified())
    }

    /// Store work runs off the async threads: a key rotation holds the store's lock for a while.
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&Store) -> T + Send + 'static) -> Result<T, AdminResp> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store)).await.map_err(|e| error(format!("worker failed: {e}")))
    }

    pub async fn handle(&self, req: AdminReq) -> AdminResp {
        let r = match req {
            AdminReq::Link => Ok(self.link()),
            AdminReq::Status => self.status().await.map(AdminResp::Status),
            AdminReq::Pcs => self.pcs().await.map(|pcs| AdminResp::Pcs { pcs }),
            AdminReq::Unlink { pc } => self.unlink(pc).await,
            AdminReq::RotateKey => self.rotate_key().await,
        };
        r.unwrap_or_else(|e| e)
    }

    fn link(&self) -> AdminResp {
        let code = self.hub.open_pairing(now_ms());
        AdminResp::Link(LinkInfo {
            code: code.code,
            expires_ms: code.expires_ms,
            window: self.hub.pairing_state().window,
            addrs: pairing::hub_addrs(&self.cfg, &self.hub.local_addrs()),
            public: self.cfg.public.as_ref().map(|p| p.to_string()),
            lan: self.lan(),
        })
    }

    async fn pcs(&self) -> Result<Vec<PcInfo>, AdminResp> {
        self.blocking(|s| {
            let stats = s.stats();
            s.pcs()
                .into_iter()
                .map(|p| PcInfo {
                    devices: s.device_count(p.host_id),
                    stored_bytes: stats.per_pc.iter().find(|(h, _)| *h == p.host_id).map_or(0, |(_, b)| *b),
                    host_id: p.host_id,
                    name: p.name,
                    user_name: p.user_name,
                    last_seen_ms: p.last_seen_ms,
                })
                .collect()
        })
        .await
    }

    async fn status(&self) -> Result<HubStatus, AdminResp> {
        let stats = self.blocking(|s| s.stats()).await?;
        Ok(HubStatus {
            name: self.store.name(),
            hub_id: self.store.hub_id(),
            public_key: seek_sync::peers::key_to_b64(&self.store.noise_keys().public),
            listening: self.hub.local_addrs().iter().map(|a| a.to_string()).collect(),
            public: self.cfg.public.as_ref().map(|p| p.to_string()),
            lan: self.lan(),
            pc_count: stats.pcs,
            devices: stats.devices,
            stored_bytes: stats.stored_bytes,
            counters: self.hub.counters().snapshot(),
            pcs: self.pcs().await?,
            pairing: self.hub.pairing_state(),
        })
    }

    /// `pc` is a host id or a name (any case); a name two PCs share must be given as a host id.
    async fn unlink(&self, pc: String) -> Result<AdminResp, AdminResp> {
        let pcs = self.blocking(|s| s.pcs()).await?;
        let found: Vec<_> = match Uuid::parse_str(pc.trim()) {
            Ok(id) => pcs.iter().filter(|p| p.host_id == id).collect(),
            Err(_) => pcs.iter().filter(|p| p.name.eq_ignore_ascii_case(pc.trim())).collect(),
        };
        let rec = match found.as_slice() {
            [] => return Err(error(format!("No linked PC is called {pc:?} (see `seek-hub pcs`)"))),
            [one] => (*one).clone(),
            many => {
                let ids: Vec<String> = many.iter().map(|p| p.host_id.to_string()).collect();
                return Err(error(format!("{} PCs are called {pc:?}; unlink one by id: {}", many.len(), ids.join(", "))));
            }
        };
        let host = rec.host_id;
        match self.blocking(move |s| s.unlink_pc(host)).await? {
            Ok(_) => {
                self.hub.close_session(host);
                tracing::info!("unlinked {} ({host})", rec.name);
                Ok(AdminResp::Unlinked { name: rec.name, host_id: host })
            }
            Err(e) => Err(error(format!("unlinking {} failed: {e}", rec.name))),
        }
    }

    /// Steps 1–3 of [`crate::rotate`], with every session closed and the store locked while files are rewritten.
    async fn rotate_key(&self) -> Result<AdminResp, AdminResp> {
        let Ok(_one) = self.rotating.try_lock() else {
            return Err(error("A key rotation is already running"));
        };
        let keys = self.cfg.keys_dir.clone();
        let (new, resumed) = rotate::begin(&keys)
            .map_err(|e| error(format!("Cannot write the new key in {}: {e}.\n{READ_ONLY_HINT}", keys.display())))?;
        tracing::info!("rotating the hub key; sessions are closed meanwhile");
        self.hub.close_sessions();
        let files = match self.blocking(move |s| s.rekey(new)).await? {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("key rotation stopped halfway: {e}");
                return Err(error(format!(
                    "The rotation stopped halfway: {e}. The hub keeps working with both keys; run rotate-key \
                     again (or restart the hub) to finish it."
                )));
            }
        };
        if let Err(e) = HubKey::commit_new(&keys) {
            return Err(error(format!(
                "Every file uses the new key, but replacing hub.key failed: {e}. The hub finishes this at its \
                 next start.\n{READ_ONLY_HINT}"
            )));
        }
        tracing::info!("hub key rotated ({files} files re-encrypted)");
        Ok(AdminResp::Rotated { files, resumed })
    }
}

/// Answers one connection: reads one request line, replies, closes.
async fn serve_conn<S: AsyncRead + AsyncWrite + Unpin>(io: S, token: Option<&str>, admin: &Admin) {
    let (rd, mut wr) = tokio::io::split(io);
    let mut rd = BufReader::new(rd.take(MAX_REQUEST as u64 + 1));
    let mut line = Vec::new();
    match tokio::time::timeout(REQUEST_TIMEOUT, rd.read_until(b'\n', &mut line)).await {
        Ok(Ok(_)) => {}
        _ => return,
    }
    let resp = if line.len() > MAX_REQUEST {
        error(format!("request longer than {MAX_REQUEST} bytes"))
    } else {
        match serde_json::from_slice::<Envelope>(&line) {
            Err(e) => error(format!("bad request: {e}")),
            Ok(env) if token.is_some_and(|t| !same(t.as_bytes(), env.token.as_bytes())) => error("wrong admin token"),
            Ok(env) => admin.handle(env.req).await,
        }
    };
    let mut out = serde_json::to_vec(&resp).unwrap_or_default();
    out.push(b'\n');
    let _ = wr.write_all(&out).await;
    let _ = wr.shutdown().await;
}

/// Compares in constant time (for equal lengths; the token's length is no secret).
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The bound admin endpoint.
pub struct AdminListener {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    #[cfg(not(unix))]
    listener: tokio::net::TcpListener,
    #[cfg(not(unix))]
    token: String,
}

impl AdminListener {
    /// Unix: `<data>/admin.sock`, replacing a stale one, mode 0600. Only the holder of the data directory's
    /// lock may bind (and so delete a socket): a second daemon never steals the endpoint.
    #[cfg(unix)]
    pub async fn bind(data: &Path, _lock: &DataLock) -> io::Result<AdminListener> {
        use std::os::unix::fs::PermissionsExt;
        let path = data.join(SOCK_FILE);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(AdminListener { listener })
    }

    /// Elsewhere: `127.0.0.1:0`; the port and a fresh token go to `<data>/admin.port`.
    #[cfg(not(unix))]
    pub async fn bind(data: &Path, _lock: &DataLock) -> io::Result<AdminListener> {
        use rand::RngCore;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let path = data.join(PORT_FILE);
        let tmp = data.join(format!("{PORT_FILE}.tmp"));
        std::fs::write(&tmp, format!("{} {token}\n", listener.local_addr()?.port()))?;
        std::fs::rename(&tmp, &path)?;
        Ok(AdminListener { listener, token })
    }

    /// Answers connections until the task is dropped.
    pub async fn run(self, admin: Arc<Admin>) {
        loop {
            match self.listener.accept().await {
                Ok((io, _)) => {
                    let admin = admin.clone();
                    #[cfg(unix)]
                    tokio::spawn(async move { serve_conn(io, None, &admin).await });
                    #[cfg(not(unix))]
                    {
                        let token = self.token.clone();
                        tokio::spawn(async move { serve_conn(io, Some(&token), &admin).await });
                    }
                }
                Err(e) => {
                    tracing::warn!("admin accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }
}

/// Removes the endpoint's file (at shutdown).
pub fn remove_endpoint(data: &Path) {
    let _ = std::fs::remove_file(endpoint_file(data));
}

fn endpoint_file(data: &Path) -> PathBuf {
    #[cfg(unix)]
    let name = SOCK_FILE;
    #[cfg(not(unix))]
    let name = PORT_FILE;
    data.join(name)
}

// ---- the command side ----

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the hub is not running (nothing answers at {})", .0.display())]
    NotRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("bad reply from the hub: {0}")]
    BadReply(String),
}

/// Sends `req` to the daemon whose data directory is `data` and waits for its reply (at most 10 s, longer
/// for a key rotation).
pub async fn request(data: &Path, req: AdminReq) -> Result<AdminResp, ClientError> {
    let limit = if req == AdminReq::RotateKey { ROTATE_TIMEOUT } else { REPLY_TIMEOUT };
    match tokio::time::timeout(limit, request_inner(data, req)).await {
        Ok(r) => r,
        Err(_) => Err(ClientError::Io(io::Error::new(io::ErrorKind::TimedOut, "the hub did not reply in time"))),
    }
}

async fn request_inner(data: &Path, req: AdminReq) -> Result<AdminResp, ClientError> {
    let file = endpoint_file(data);
    let not_running = |e: io::Error| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::NotRunning(file.clone()),
        _ => ClientError::Io(e),
    };
    #[cfg(unix)]
    let (io, token) = (tokio::net::UnixStream::connect(&file).await.map_err(not_running)?, String::new());
    #[cfg(not(unix))]
    let (io, token) = {
        let text = std::fs::read_to_string(&file).map_err(not_running)?;
        let damaged = || ClientError::BadReply(format!("{} is damaged", file.display()));
        let (port, token) = text.trim().split_once(' ').ok_or_else(damaged)?;
        let port: u16 = port.parse().map_err(|_| damaged())?;
        (tokio::net::TcpStream::connect(("127.0.0.1", port)).await.map_err(not_running)?, token.to_string())
    };
    exchange(io, &Envelope { token, req }).await
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(io: S, env: &Envelope) -> Result<AdminResp, ClientError> {
    let (rd, mut wr) = tokio::io::split(io);
    let mut line = serde_json::to_vec(env).map_err(io::Error::other)?;
    line.push(b'\n');
    wr.write_all(&line).await?;
    wr.flush().await?;
    let mut reply = Vec::new();
    BufReader::new(rd.take(MAX_REPLY)).read_until(b'\n', &mut reply).await?;
    if reply.is_empty() {
        return Err(ClientError::BadReply("the hub closed the connection without a reply".into()));
    }
    serde_json::from_slice(&reply).map_err(|e| ClientError::BadReply(e.to_string()))
}

#[cfg(test)]
mod tests;
