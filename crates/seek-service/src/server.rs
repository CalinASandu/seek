//! Named-pipe server: one task per connection, requests handled concurrently.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine;
use seek_core::{
    decode_index, materialize, normalize_unc, query, search, Device, DeviceId, DeviceIdentity,
    DeviceKind, DeviceState, Hit, SearchDevice, SortKey,
};
use seek_ipc::{
    create_server, read_frame, write_frame, Envelope, IndexProgress, Request, Response, StatusInfo,
    UserVolume, PROTOCOL_VERSION,
};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::mpsc;

use crate::monitor;
use crate::state::{now_ms, Snapshot, State};

const MAX_ROWS_PER_FETCH: u32 = 500;
const KEPT_SEARCHES: usize = 2;

struct StoredSearch {
    id: u64,
    /// Device id and index epoch of every device searched, in hit `dev` order.
    devices: Vec<(DeviceId, u64)>,
    hits: Arc<Vec<Hit>>,
}

#[derive(Default)]
struct Conn {
    generation: AtomicU64,
    searches: Mutex<VecDeque<StoredSearch>>,
}

/// Search ids are unique across connections and service restarts (seeded from the clock), so a client
/// that reconnects never mistakes a new result set for a cached old one. Stays below 2^53 for JS.
static NEXT_SEARCH_ID: AtomicU64 = AtomicU64::new(0);

fn next_search_id() -> u64 {
    let seed = now_ms().max(0) as u64 * 1000;
    NEXT_SEARCH_ID.fetch_max(seed, Ordering::SeqCst);
    NEXT_SEARCH_ID.fetch_add(1, Ordering::SeqCst) + 1
}

pub async fn serve(state: Arc<State>, mut shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
    let name = state.cfg.pipe_name.clone();
    let mut server = create_server(&name, true)?;
    tracing::info!("listening on {name}");
    loop {
        tokio::select! {
            r = server.connect() => r?,
            _ = shutdown.changed() => return Ok(()),
        }
        let conn = std::mem::replace(&mut server, create_server(&name, false)?);
        tokio::spawn(handle_connection(conn, state.clone()));
    }
}

async fn handle_connection(pipe: NamedPipeServer, state: Arc<State>) {
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let (tx, mut rx) = mpsc::channel::<Envelope<Response>>(64);
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });
    let conn = Arc::new(Conn::default());
    let mut tasks = Vec::new();
    while let Ok(Some(env)) = read_frame::<_, Envelope<Request>>(&mut rd).await {
        if env.body == Request::Subscribe {
            let mut events = state.events.subscribe();
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if tx.send(Envelope { id: 0, body: Response::Event { event } }).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }));
            continue;
        }
        let (state, conn, tx) = (state.clone(), conn.clone(), tx.clone());
        tasks.push(tokio::spawn(async move {
            let body = handle(state, conn, env.body).await;
            let _ = tx.send(Envelope { id: env.id, body }).await;
        }));
        tasks.retain(|t| !t.is_finished());
    }
    for t in tasks {
        t.abort();
    }
    drop(tx);
    let _ = writer.await;
}

fn err(message: impl Into<String>) -> Response {
    Response::Error { message: message.into() }
}

async fn handle(state: Arc<State>, conn: Arc<Conn>, req: Request) -> Response {
    match req {
        Request::Hello { protocol_version } => {
            if protocol_version != PROTOCOL_VERSION {
                return err(format!(
                    "protocol mismatch: app speaks v{protocol_version}, service speaks v{PROTOCOL_VERSION}"
                ));
            }
            Response::Hello {
                protocol_version: PROTOCOL_VERSION,
                service_version: env!("CARGO_PKG_VERSION").into(),
            }
        }
        Request::Search { query, sort, descending } => run_search(state, conn, query, sort, descending).await,
        Request::FetchRows { search_id, offset, limit } => fetch_rows(state, conn, search_id, offset, limit).await,
        Request::ListDevices => Response::Devices { devices: state.registry.read().unwrap().devices.clone() },
        Request::RenameDevice { id, name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return err("name cannot be empty");
            }
            if state.update_device(id, |d| d.display_name = name) {
                state.invalidate();
                Response::Ok
            } else {
                err("unknown device")
            }
        }
        Request::ForgetDevice { id } => forget(&state, id),
        Request::RescanDevice { id } => match state.device(id) {
            None => err("unknown device"),
            Some(d) if is_app_fed(&state, &d) => err("this device is scanned by the app (network shares are rescanned by the app)"),
            Some(_) => match monitor::rescan(&state, id) {
                Ok(()) => Response::Ok,
                Err(e) => err(e),
            },
        },
        Request::AddNetworkShare { unc } => add_share(&state, &unc),
        Request::PushShareIndex { device_id, data_b64 } => {
            tokio::task::spawn_blocking(move || push_share(&state, device_id, &data_b64))
                .await
                .unwrap_or_else(|e| err(e.to_string()))
        }
        Request::AddUserVolume { volume } => add_user_volume(&state, volume),
        Request::SetShareState { device_id, online } => {
            match state.device(device_id) {
                Some(d) if is_app_fed(&state, &d) => {}
                Some(_) => return err("device is indexed by the service"),
                None => return err("unknown device"),
            }
            let ok = state.update_device(device_id, |d| {
                d.state = if online { DeviceState::Online } else { DeviceState::Offline };
                d.last_seen = now_ms();
            });
            state.invalidate();
            if ok { Response::Ok } else { err("unknown device") }
        }
        Request::Status => Response::Status(status(&state)),
        Request::Subscribe => Response::Ok,
    }
}

async fn run_search(state: Arc<State>, conn: Arc<Conn>, q: String, sort: SortKey, descending: bool) -> Response {
    let expr = match query::parse(&q) {
        Ok(e) => e,
        Err(e) => return Response::SearchError { message: e.message, position: e.position },
    };
    let my_gen = conn.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let c2 = conn.clone();
    let result = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let snap = state.snapshot();
        let guards: Vec<_> = snap.iter().map(|s| s.index.read().unwrap()).collect();
        let devs: Vec<SearchDevice> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| SearchDevice { device: &s.device, index: g, mount: &s.device.last_mount_point, online: s.online })
            .collect();
        let cancelled = || c2.generation.load(Ordering::SeqCst) != my_gen;
        let hits = search(&devs, &expr, sort, descending, &cancelled)?;
        let devices = snap.iter().zip(&guards).map(|(s, g)| (s.device.id, g.epoch())).collect();
        Some((hits, devices, started.elapsed().as_secs_f64() * 1000.0))
    })
    .await;
    match result {
        Ok(Some((hits, devices, elapsed_ms))) => {
            let id = next_search_id();
            let total = hits.len() as u64;
            let mut searches = conn.searches.lock().unwrap();
            searches.push_back(StoredSearch { id, devices, hits: Arc::new(hits) });
            while searches.len() > KEPT_SEARCHES {
                searches.pop_front();
            }
            Response::SearchStarted { search_id: id, total, elapsed_ms }
        }
        Ok(None) => err("cancelled by a newer search"),
        Err(e) => err(format!("search failed: {e}")),
    }
}

async fn fetch_rows(state: Arc<State>, conn: Arc<Conn>, search_id: u64, offset: u32, limit: u32) -> Response {
    let found = {
        let searches = conn.searches.lock().unwrap();
        searches.iter().find(|s| s.id == search_id).map(|s| (s.devices.clone(), s.hits.clone()))
    };
    let Some((devices, hits)) = found else { return err("unknown or expired search") };
    let limit = limit.min(MAX_ROWS_PER_FETCH) as usize;
    let start = (offset as usize).min(hits.len());
    let end = (start + limit).min(hits.len());
    let rows = tokio::task::spawn_blocking(move || {
        let snap: Vec<Snapshot> = state.snapshot();
        let guards: Vec<_> = snap.iter().map(|s| s.index.read().unwrap()).collect();
        // Map the device order captured at search time onto the current snapshot; a device that is
        // gone or whose rows were renumbered (compaction, rescan) cannot serve its old hits.
        let order: Vec<Option<usize>> = devices
            .iter()
            .map(|(id, epoch)| {
                snap.iter().zip(&guards).position(|(s, g)| s.device.id == *id && g.epoch() == *epoch)
            })
            .collect();
        let devs: Vec<SearchDevice> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| SearchDevice { device: &s.device, index: g, mount: &s.device.last_mount_point, online: s.online })
            .collect();
        // All or nothing: rows are positional on the client, so a page must never have gaps.
        hits[start..end]
            .iter()
            .map(|h| {
                let dev = (*order.get(h.dev as usize)?)?;
                Some(materialize(&devs, Hit { dev: dev as u16, row: h.row }))
            })
            .collect::<Option<Vec<_>>>()
    })
    .await;
    match rows {
        Ok(Some(rows)) => Response::Rows { search_id, offset: start as u32, rows },
        Ok(None) => err("results changed; search again"),
        Err(e) => err(e.to_string()),
    }
}

fn forget(state: &Arc<State>, id: DeviceId) -> Response {
    let Some(d) = state.device(id) else { return err("unknown device") };
    if matches!(d.state, DeviceState::Online | DeviceState::Scanning) {
        return err("only offline devices can be forgotten");
    }
    state.registry.write().unwrap().remove(id);
    state.indexes.write().unwrap().remove(&id);
    state.dirty.lock().unwrap().remove(&id);
    let _ = std::fs::remove_file(state.index_path(id));
    state.save_registry();
    state.emit(seek_ipc::Event::DevicesChanged);
    state.invalidate();
    Response::Ok
}

fn add_share(state: &Arc<State>, unc: &str) -> Response {
    let unc = normalize_unc(unc);
    let parts: Vec<&str> = unc.trim_start_matches('\\').split('\\').collect();
    if !unc.starts_with("\\\\") || parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return err("expected a share path like \\\\server\\share");
    }
    let identity = DeviceIdentity::Share { unc: unc.clone() };
    if let Some(d) = state.registry.read().unwrap().find_by_identity(&identity) {
        return Response::Device { device: d.clone() };
    }
    let now = now_ms();
    let device = Device {
        id: uuid::Uuid::new_v4(),
        identity,
        display_name: parts[parts.len() - 1].to_string(),
        label: String::new(),
        filesystem: "SMB".into(),
        kind: DeviceKind::Network,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: format!("{unc}\\"),
        indexed_by_host: state.host_id(),
        first_seen: now,
        last_seen: now,
        index_updated_at: 0,
        state: DeviceState::Offline,
        entry_count: 0,
        error: None,
    };
    state.registry.write().unwrap().add(device.clone());
    state.save_registry();
    state.emit(seek_ipc::Event::DevicesChanged);
    Response::Device { device }
}

/// Devices whose index comes from the app: network shares and volumes only the user's session sees.
fn is_app_fed(state: &State, d: &Device) -> bool {
    d.kind == DeviceKind::Network || state.user_volumes.lock().unwrap().contains(&d.id)
}

fn add_user_volume(state: &Arc<State>, v: UserVolume) -> Response {
    if !matches!(v.identity, DeviceIdentity::Volume { .. }) {
        return err("expected a volume identity");
    }
    let seen_by_service = state.mounted.lock().unwrap().values().any(|m| m.identity == v.identity);
    if seen_by_service {
        return Response::Ok;
    }
    let info = seek_scan::VolumeInfo {
        mount_point: v.mount_point.clone(),
        identity: v.identity.clone(),
        label: v.label.clone(),
        filesystem: v.filesystem.clone(),
        kind: v.kind,
        total_bytes: v.total_bytes,
        free_bytes: v.free_bytes,
    };
    let existing = state.registry.read().unwrap().find_by_identity(&v.identity).map(|d| d.id);
    let id = existing.unwrap_or_else(|| monitor::register_volume(state, &info));
    state.user_volumes.lock().unwrap().insert(id);
    state.update_device(id, |d| {
        d.last_mount_point = v.mount_point;
        d.label = v.label;
        d.filesystem = v.filesystem;
        d.kind = v.kind;
        d.total_bytes = v.total_bytes;
        d.free_bytes = v.free_bytes;
        d.last_seen = now_ms();
    });
    match state.device(id) {
        Some(device) => Response::Device { device },
        None => err("unknown device"),
    }
}

fn push_share(state: &Arc<State>, id: DeviceId, data_b64: &str) -> Response {
    match state.device(id) {
        Some(d) if is_app_fed(state, &d) => {}
        Some(_) => return err("device is indexed by the service"),
        None => return err("unknown device"),
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
        Ok(b) => b,
        Err(e) => return err(format!("bad base64: {e}")),
    };
    let ix = match decode_index(&bytes) {
        Ok((_, ix)) => ix,
        Err(e) => return err(format!("bad index: {e}")),
    };
    let count = ix.live_count() as u64;
    state.indexes.write().unwrap().insert(id, Arc::new(std::sync::RwLock::new(ix)));
    state.mark_dirty(id);
    state.save_index(id);
    state.update_device(id, |d| {
        d.state = DeviceState::Online;
        d.entry_count = count;
        d.index_updated_at = now_ms();
        d.last_seen = now_ms();
        d.error = None;
    });
    state.invalidate();
    Response::Ok
}

fn status(state: &Arc<State>) -> StatusInfo {
    let reg = state.registry.read().unwrap();
    let online = reg.devices.iter().filter(|d| matches!(d.state, DeviceState::Online | DeviceState::Scanning)).count();
    StatusInfo {
        service_version: env!("CARGO_PKG_VERSION").into(),
        total_entries: reg.devices.iter().map(|d| d.entry_count).sum(),
        devices_online: online as u32,
        devices_offline: (reg.devices.len() - online) as u32,
        indexing: state
            .scanning
            .lock()
            .unwrap()
            .iter()
            .map(|(id, n)| IndexProgress { device_id: *id, scanned: *n, done: false })
            .collect(),
    }
}
