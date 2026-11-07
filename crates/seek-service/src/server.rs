//! Named-pipe server: one task per connection, requests handled concurrently.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine;
use seek_core::{
    decode_index, materialize, normalize_unc, query, search, Device, DeviceId, DeviceIdentity,
    DeviceKind, DeviceIndex, DeviceState, FolderTotals, Hit, ResultRow, SearchDevice, SortKey,
};
use seek_ipc::{
    create_server, read_frame, write_frame, Envelope, IndexProgress, PcInfo, PcSummary, Request,
    Response, SizeGroup, StatusInfo, UserVolume, PROTOCOL_VERSION,
};
use seek_sync::peers::PeerState;
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::mpsc;

use crate::peers::PeersRuntime;
use crate::state::{now_ms, Snapshot, State};
use crate::{access, monitor};

const MAX_ROWS_PER_FETCH: u32 = 500;
const KEPT_SEARCHES: usize = 2;
const MAX_SPACE_DEPTH: u8 = 8;
const MAX_SPACE_FILES: u32 = 1000;
/// Most files in one same-size page (every page scans the whole index, so pages must be big).
const MAX_SAME_SIZE_FILES_PER_PAGE: u32 = 20_000;
const FOLDER_GONE: &str = "That folder no longer exists";

struct StoredSearch {
    id: u64,
    /// Device id and index epoch of every device searched, in hit `dev` order.
    devices: Vec<(DeviceId, u64)>,
    hits: Arc<Vec<Hit>>,
}

/// Hits kept for clients, on all connections.
static STORED_HITS: AtomicU64 = AtomicU64::new(0);

impl Drop for StoredSearch {
    fn drop(&mut self) {
        STORED_HITS.fetch_sub(self.hits.len() as u64, Ordering::Relaxed);
    }
}

pub(crate) fn stored_result_bytes() -> u64 {
    STORED_HITS.load(Ordering::Relaxed) * std::mem::size_of::<Hit>() as u64
}

#[derive(Default)]
struct Conn {
    generation: AtomicU64,
    searches: Mutex<VecDeque<StoredSearch>>,
    /// The client's user SID and the folders it may not see (see `access`).
    sid: String,
    elevated: bool,
    hidden: Vec<String>,
}

/// Search ids are unique across connections and service restarts (seeded from the clock), so a client
/// that reconnects never mistakes a new result set for a cached old one. Stays below 2^53 for JS.
static NEXT_SEARCH_ID: AtomicU64 = AtomicU64::new(0);

fn next_search_id() -> u64 {
    let seed = now_ms().max(0) as u64 * 1000;
    NEXT_SEARCH_ID.fetch_max(seed, Ordering::SeqCst);
    NEXT_SEARCH_ID.fetch_add(1, Ordering::SeqCst) + 1
}

pub async fn serve(
    state: Arc<State>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
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
    let caller = state.source.caller(&pipe);
    let hidden = access::hidden_folders(&caller, state.source.profiles());
    tracing::debug!(
        "client {} (elevated: {}), {} folder(s) hidden",
        caller.sid,
        caller.elevated,
        hidden.len()
    );
    let conn = Arc::new(Conn {
        elevated: caller.elevated,
        sid: caller.sid,
        hidden,
        ..Conn::default()
    });
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let (tx, mut rx) = mpsc::channel::<Envelope<Response>>(64);
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });
    let mut tasks = Vec::new();
    // stored results are row numbers: they mean nothing once their indexes have left memory
    let mut parked = state.parked_gen.subscribe();
    let stored = conn.clone();
    tasks.push(tokio::spawn(async move {
        while parked.changed().await.is_ok() {
            stored.searches.lock().unwrap().clear();
        }
    }));
    while let Ok(Some(env)) = read_frame::<_, Envelope<Request>>(&mut rd).await {
        if env.body == Request::Subscribe {
            let mut events = state.events.subscribe();
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if tx
                                .send(Envelope {
                                    id: 0,
                                    body: Response::Event { event },
                                })
                                .await
                                .is_err()
                            {
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
    Response::Error {
        message: message.into(),
    }
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
        Request::Search {
            query,
            sort,
            descending,
        } => run_search(state, conn, query, sort, descending).await,
        Request::FetchRows {
            search_id,
            offset,
            limit,
        } => fetch_rows(state, conn, search_id, offset, limit).await,
        Request::Wake => {
            tokio::task::spawn_blocking(move || state.wake());
            Response::Ok
        }
        Request::Memory => tokio::task::spawn_blocking(move || Response::Memory { report: state.memory_report() })
            .await
            .unwrap_or_else(|e| err(format!("memory report failed: {e}"))),
        Request::ListDevices => Response::Devices {
            devices: state.visible_devices(),
        },
        Request::RenameDevice { id, .. }
        | Request::ForgetDevice { id }
        | Request::RescanDevice { id }
            if state.device(id).and_then(|d| state.remote_owner(&d)).is_some() =>
        {
            let owner = state.device(id).and_then(|d| state.remote_owner(&d)).unwrap_or_default();
            err(format!("this drive belongs to {owner}"))
        }
        Request::RenameDevice { id, name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return err("name cannot be empty");
            }
            if state.update_device(id, |d| d.display_name = name) {
                state.invalidate();
                Response::Ok
            } else {
                err("unknown drive")
            }
        }
        Request::ForgetDevice { id } => forget(&state, id),
        Request::RescanDevice { id } => match state.device(id) {
            None => err("unknown drive"),
            Some(d) if is_app_fed(&state, &d) => {
                err("this drive is scanned by the app (network shares are rescanned by the app)")
            }
            Some(_) => match monitor::rescan(&state, id) {
                Ok(()) => Response::Ok,
                Err(e) => err(e),
            },
        },
        Request::AddNetworkShare { unc } => add_share(&state, &unc),
        Request::PushShareIndex {
            device_id,
            data_b64,
        } => tokio::task::spawn_blocking(move || push_share(&state, device_id, &data_b64))
            .await
            .unwrap_or_else(|e| err(e.to_string())),
        Request::AddUserVolume { volume } => add_user_volume(&state, volume),
        Request::SetShareState { device_id, online } => {
            match state.device(device_id) {
                Some(d) if is_app_fed(&state, &d) => {}
                Some(_) => return err("drive is indexed by the service"),
                None => return err("unknown drive"),
            }
            let ok = state.update_device(device_id, |d| {
                d.state = if online {
                    DeviceState::Online
                } else {
                    DeviceState::Offline
                };
                d.last_seen = now_ms();
            });
            state.invalidate();
            if ok {
                Response::Ok
            } else {
                err("unknown drive")
            }
        }
        Request::SpaceTree { device_id, path, depth } => {
            space(state, conn, device_id, path, move |sd, t, folder| Response::SpaceTree {
                node: seek_core::space_tree(sd.index, t, folder, depth.min(MAX_SPACE_DEPTH)),
            })
            .await
        }
        Request::SpaceLargest { device_id, path, limit } => {
            space(state, conn, device_id, path, move |sd, t, folder| {
                let rows = seek_core::largest_files(sd.index, t, folder, limit.min(MAX_SPACE_FILES) as usize);
                let devs = std::slice::from_ref(sd);
                Response::SpaceFiles {
                    files: rows.into_iter().map(|row| materialize(devs, Hit { dev: 0, row })).collect(),
                }
            })
            .await
        }
        Request::SpaceTypes { device_id, path } => {
            space(state, conn, device_id, path, |sd, t, folder| Response::SpaceTypes {
                types: seek_core::type_totals(sd.index, t, folder),
            })
            .await
        }
        Request::SameSizeFiles { min_size, device_ids, cursor, limit, same_name } => {
            same_size_files(state, conn, min_size, device_ids, cursor, limit, same_name).await
        }
        Request::StaleData => tokio::task::spawn_blocking(move || Response::StaleData { files: state.stale_data() })
            .await
            .unwrap_or_else(|e| err(format!("listing stale data failed: {e}"))),
        Request::FolderFacts { paths } => folder_facts(state, conn, paths).await,
        Request::Status => Response::Status(status(&state)),
        Request::Subscribe => Response::Ok,
        Request::StartPairing
        | Request::DiscoveredPcs
        | Request::JoinPairing { .. }
        | Request::LinkHub { .. }
        | Request::UnlinkHub
        | Request::HubStatus
        | Request::ListPeers
        | Request::Unlink { .. }
        | Request::RenamePeer { .. }
        | Request::SyncNow
        | Request::SetSharing { .. }
        | Request::SetDeviceShared { .. } => match state.peers_rt.get().cloned() {
            Some(rt) => sync_request(&state, &rt, &conn, req).await,
            None => err("sync is disabled"),
        },
    }
}

async fn sync_request(state: &Arc<State>, rt: &Arc<PeersRuntime>, conn: &Conn, req: Request) -> Response {
    match req {
        Request::StartPairing => {
            let c = rt.start_pairing(&conn.sid);
            Response::PairingCode {
                code: c.code,
                expires_ms: c.expires_ms,
            }
        }
        Request::DiscoveredPcs => Response::Pcs {
            pcs: rt
                .discovered()
                .into_iter()
                .map(|d| PcInfo {
                    host_id: d.host_id,
                    name: d.name,
                    addr: d.addr,
                })
                .collect(),
        },
        Request::JoinPairing { addr, code } => {
            let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
            if code.len() != 6 {
                return err("the code has 6 digits");
            }
            match rt.join(addr.trim(), &code, &conn.sid).await {
                Ok(_) => Response::Ok,
                Err(e) => err(e),
            }
        }
        Request::LinkHub { addr, code } => match rt.link_hub(addr.trim(), &code, &conn.sid).await {
            Ok(name) => Response::HubLinked { name },
            Err(e) => err(e),
        },
        Request::UnlinkHub => match rt.unlink_hub().await {
            Ok(()) => Response::Ok,
            Err(e) => err(e),
        },
        Request::HubStatus => Response::Hub { hub: rt.hub_status() },
        Request::ListPeers => peers_info(state, rt),
        Request::Unlink { host_id } => match rt.unlink(host_id).await {
            Ok(()) => Response::Ok,
            Err(e) => err(e),
        },
        Request::RenamePeer { host_id, name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return err("name cannot be empty");
            }
            if let Err(e) = rt.rename_peer(host_id, name) {
                return err(&e);
            }
            state.invalidate();
            Response::Ok
        }
        Request::SyncNow => {
            rt.sync_now();
            Response::Ok
        }
        Request::SetSharing { enabled } => {
            rt.set_sharing(enabled);
            Response::Ok
        }
        Request::SetDeviceShared { device_id, shared } => match state.device(device_id) {
            Some(d) if state.remote_owner(&d).is_none() => {
                rt.set_device_shared(device_id, shared);
                Response::Ok
            }
            Some(_) => err("only this device's drives can be shared"),
            None => err("unknown drive"),
        },
        _ => err("not a sync request"),
    }
}

fn peers_info(state: &Arc<State>, rt: &Arc<PeersRuntime>) -> Response {
    let peers = state.peers.lock().unwrap();
    let last_sync = peers.file.peers.iter().map(|p| p.last_sync_ms).max().unwrap_or(0);
    Response::Peers {
        this_pc: PcSummary {
            host_id: state.host_id(),
            name: state.pc_name(),
            online: true,
            last_sync_ms: last_sync,
            needs_relink: false,
        },
        peers: peers
            .file
            .peers
            .iter()
            .filter(|p| p.role == seek_sync::Role::Pc)
            .map(|p| PcSummary {
                host_id: p.host_id,
                name: p.name.clone(),
                online: rt.peer_online(p),
                last_sync_ms: p.last_sync_ms,
                needs_relink: p.state == PeerState::NeedsRelink,
            })
            .collect(),
        sharing: peers.file.sharing,
        unshared: peers.file.unshared.clone(),
    }
}

async fn run_search(
    state: Arc<State>,
    conn: Arc<Conn>,
    q: String,
    sort: SortKey,
    descending: bool,
) -> Response {
    let expr = match query::parse(&q) {
        Ok(e) => e,
        Err(e) => {
            return Response::SearchError {
                message: e.message,
                position: e.position,
            }
        }
    };
    let my_gen = conn.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let c2 = conn.clone();
    let result = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let snap = state.snapshot("search");
        let guards: Vec<_> = snap.iter().map(|s| s.index.read().unwrap()).collect();
        let devs: Vec<SearchDevice> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| SearchDevice {
                device: &s.device,
                index: g,
                mount: &s.device.last_mount_point,
                online: s.online,
                pc: &s.pc,
            })
            .collect();
        let cancelled = || c2.generation.load(Ordering::SeqCst) != my_gen;
        let mut hits = search(&devs, &expr, sort, descending, &cancelled)?;
        let masks: Vec<_> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| {
                let hidden = state.hidden_for(&s.device, &c2.sid, c2.elevated, &c2.hidden);
                state.hidden_mask(&s.device, g, &c2.sid, &hidden)
            })
            .collect();
        if masks.iter().any(Option::is_some) {
            hits.retain(|h| {
                masks[h.dev as usize]
                    .as_ref()
                    .is_none_or(|m| !m[h.row as usize])
            });
        }
        let devices = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| (s.device.id, g.epoch()))
            .collect();
        Some((hits, devices, started.elapsed().as_secs_f64() * 1000.0))
    })
    .await;
    match result {
        Ok(Some((hits, devices, elapsed_ms))) => {
            let id = next_search_id();
            let total = hits.len() as u64;
            STORED_HITS.fetch_add(total, Ordering::Relaxed);
            let mut searches = conn.searches.lock().unwrap();
            searches.push_back(StoredSearch {
                id,
                devices,
                hits: Arc::new(hits),
            });
            while searches.len() > KEPT_SEARCHES {
                searches.pop_front();
            }
            Response::SearchStarted {
                search_id: id,
                total,
                elapsed_ms,
            }
        }
        Ok(None) => err("cancelled by a newer search"),
        Err(e) => err(format!("search failed: {e}")),
    }
}

/// Runs `f` for a Space view request: the device's index, the caller's folder totals and the folder
/// `path` names (`None` = the drive root), off the async runtime.
async fn space(
    state: Arc<State>,
    conn: Arc<Conn>,
    device_id: DeviceId,
    path: String,
    f: impl FnOnce(&SearchDevice, &FolderTotals, Option<u32>) -> Response + Send + 'static,
) -> Response {
    tokio::task::spawn_blocking(move || {
        let snap = state.snapshot("space view");
        let Some(s) = snap.iter().find(|s| s.device.id == device_id) else {
            return err("Unknown drive");
        };
        let ix = s.index.read().unwrap();
        let mount = s.device.last_mount_point.as_str();
        let hidden = state.hidden_for(&s.device, &conn.sid, conn.elevated, &conn.hidden);
        let mask = state.hidden_mask(&s.device, &ix, &conn.sid, &hidden);
        let Some(folder) = space_folder(&ix, mount, &path) else {
            return err(FOLDER_GONE);
        };
        if let (Some(row), Some(m)) = (folder, &mask) {
            if m[row as usize] {
                return err(FOLDER_GONE);
            }
        }
        let totals = state.space_totals(&s.device, &ix, &conn.sid, mask.as_deref().map(Vec::as_slice));
        let sd = SearchDevice {
            device: &s.device,
            index: &ix,
            mount,
            online: s.online,
            pc: &s.pc,
        };
        f(&sd, &totals, folder)
    })
    .await
    .unwrap_or_else(|e| err(format!("space view failed: {e}")))
}

/// The first requested drive that exists (is in `visible`) but has no loaded index.
fn first_unloaded(requested: &[DeviceId], visible: &[DeviceId], loaded: &[DeviceId]) -> Option<DeviceId> {
    requested.iter().copied().find(|id| visible.contains(id) && !loaded.contains(id))
}

/// How many of the leading `(size, files)` groups make one page: whole groups only, up to
/// `MAX_SAME_SIZE_FILES_PER_PAGE` or `limit` files (the smaller), and always at least one group.
fn groups_in_page(sizes: &[(u64, u64)], limit: u32) -> usize {
    let max_files = u64::from(limit.min(MAX_SAME_SIZE_FILES_PER_PAGE));
    let (mut taken, mut files) = (0, 0u64);
    for &(_, n) in sizes {
        if taken > 0 && files + n > max_files {
            break;
        }
        taken += 1;
        files += n;
    }
    taken
}

/// One page of files that share a size across `device_ids` (first stage of the duplicate finder). Reads the
/// index only and keeps nothing between pages: each call recomputes from the cursor, so an index change
/// between pages can shift results, but every page's sizes are strictly below the previous page's.
async fn same_size_files(
    state: Arc<State>,
    conn: Arc<Conn>,
    min_size: u64,
    device_ids: Vec<DeviceId>,
    cursor: Option<u64>,
    limit: u32,
    same_name: bool,
) -> Response {
    if device_ids.is_empty() {
        return Response::SameSizeFiles { groups: Vec::new(), next: None };
    }
    tokio::task::spawn_blocking(move || {
        let mut snap = state.snapshot("same size");
        // ids that are no drive at all are skipped, but a drive whose index did not load is an error:
        // an empty answer would read as "no duplicates"
        let visible: Vec<DeviceId> = state.visible_devices().iter().map(|d| d.id).collect();
        let loaded: Vec<DeviceId> = snap.iter().map(|s| s.device.id).collect();
        if let Some(id) = first_unloaded(&device_ids, &visible, &loaded) {
            let name = state.device(id).map_or_else(|| "A drive".to_string(), |d| d.display_name);
            return err(format!("{name} could not be read right now, so its files can't be compared"));
        }
        // the order is stable whatever the snapshot's order
        snap.retain(|s| device_ids.contains(&s.device.id));
        snap.sort_by_key(|s| s.device.id);
        let guards: Vec<_> = snap.iter().map(|s| s.index.read().unwrap()).collect();
        let masks: Vec<_> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| {
                let hidden = state.hidden_for(&s.device, &conn.sid, conn.elevated, &conn.hidden);
                state.hidden_mask(&s.device, g, &conn.sid, &hidden)
            })
            .collect();
        let mask_of = |d: usize| masks[d].as_deref().map(Vec::as_slice);

        // files per size over all chosen drives; only sizes held by two or more files are candidates.
        // With `same_name`, only (size, name) pairs held by two or more files count.
        let mut counts: HashMap<u64, u64> = HashMap::new();
        let mut names: HashSet<(u64, String)> = HashSet::new();
        if same_name {
            let mut by_name: HashMap<(u64, String), u64> = HashMap::new();
            for (d, g) in guards.iter().enumerate() {
                for (key, n) in seek_core::name_size_counts(g, mask_of(d), min_size) {
                    *by_name.entry(key).or_default() += u64::from(n);
                }
            }
            for (key, n) in by_name.into_iter().filter(|&(_, n)| n >= 2) {
                *counts.entry(key.0).or_default() += n;
                names.insert(key);
            }
        } else {
            for (d, g) in guards.iter().enumerate() {
                for (size, n) in seek_core::sizes_with_files(g, mask_of(d), min_size) {
                    *counts.entry(size).or_default() += u64::from(n);
                }
            }
        }
        let mut sizes: Vec<(u64, u64)> = counts
            .into_iter()
            .filter(|&(size, n)| n >= 2 && cursor.is_none_or(|c| size <= c))
            .collect();
        sizes.sort_unstable_by_key(|&(size, _)| std::cmp::Reverse(size));

        let taken = groups_in_page(&sizes, limit);
        let next = sizes.get(taken).map(|&(size, _)| size);
        let page: Vec<u64> = sizes[..taken].iter().map(|&(size, _)| size).collect();
        let wanted: HashSet<u64> = page.iter().copied().collect();

        let devs: Vec<SearchDevice> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| SearchDevice {
                device: &s.device,
                index: g,
                mount: &s.device.last_mount_point,
                online: s.online,
                pc: &s.pc,
            })
            .collect();
        names.retain(|k| wanted.contains(&k.0));
        let mut by_size: HashMap<u64, Vec<ResultRow>> = HashMap::new();
        for (d, g) in guards.iter().enumerate() {
            let rows_by_size = if same_name {
                seek_core::files_of_name_size(g, mask_of(d), &names)
            } else {
                seek_core::files_of_size(g, mask_of(d), &wanted)
            };
            for (size, rows) in rows_by_size {
                by_size
                    .entry(size)
                    .or_default()
                    .extend(rows.into_iter().map(|row| materialize(&devs, Hit { dev: d as u16, row })));
            }
        }
        let groups = page
            .into_iter()
            .map(|size| SizeGroup { size, files: by_size.remove(&size).unwrap_or_default() })
            .collect();
        Response::SameSizeFiles { groups, next }
    })
    .await
    .unwrap_or_else(|e| err(format!("same-size search failed: {e}")))
}

/// `Some(None)` for the drive root, `Some(Some(row))` for a live folder, `None` when there is no such folder.
/// What the index knows about each folder in `paths`, for the folders on this PC's online drives.
/// Hidden rows are handled as the space view does; a hidden folder is not found.
async fn folder_facts(state: Arc<State>, conn: Arc<Conn>, paths: Vec<String>) -> Response {
    tokio::task::spawn_blocking(move || {
        let me = state.host_id();
        let mut facts: Vec<Option<seek_core::FolderFact>> = vec![None; paths.len()];
        for s in state.snapshot("folder facts") {
            if s.device.indexed_by_host != me || s.device.state != DeviceState::Online {
                continue;
            }
            let mount = s.device.last_mount_point.as_str();
            let prefix = format!("{}\\", mount.trim_end_matches('\\').to_lowercase());
            let mine: Vec<usize> = (0..paths.len()).filter(|&i| paths[i].to_lowercase().starts_with(&prefix)).collect();
            if mine.is_empty() {
                continue;
            }
            let ix = s.index.read().unwrap();
            let hidden = state.hidden_for(&s.device, &conn.sid, conn.elevated, &conn.hidden);
            let mask = state.hidden_mask(&s.device, &ix, &conn.sid, &hidden);
            let wanted: Vec<&str> = mine.iter().map(|&i| paths[i].as_str()).collect();
            let found = ix.find_folders(mount, &wanted);
            let visible: Vec<(usize, u32)> = mine
                .iter()
                .zip(found)
                .filter_map(|(&i, row)| row.map(|r| (i, r)))
                .filter(|&(_, r)| !mask.as_ref().is_some_and(|m| m[r as usize]))
                .collect();
            let rows: Vec<u32> = visible.iter().map(|&(_, r)| r).collect();
            let got = seek_core::folder_facts(&ix, mask.as_deref().map(Vec::as_slice), &rows);
            for ((i, _), f) in visible.into_iter().zip(got) {
                facts[i] = Some(f);
            }
        }
        Response::FolderFacts { facts }
    })
    .await
    .unwrap_or_else(|e| err(format!("folder facts failed: {e}")))
}

fn space_folder(ix: &DeviceIndex, mount: &str, path: &str) -> Option<Option<u32>> {
    let path = path.trim_end_matches('\\');
    if path.eq_ignore_ascii_case(mount.trim_end_matches('\\')) {
        return Some(None);
    }
    ix.find_folders(mount, &[path]).pop().flatten().map(Some)
}

async fn fetch_rows(
    state: Arc<State>,
    conn: Arc<Conn>,
    search_id: u64,
    offset: u32,
    limit: u32,
) -> Response {
    let found = {
        let searches = conn.searches.lock().unwrap();
        searches
            .iter()
            .find(|s| s.id == search_id)
            .map(|s| (s.devices.clone(), s.hits.clone()))
    };
    let Some((devices, hits)) = found else {
        // also what a client sees when idle unloading dropped its results: it searches again
        return err("results changed; search again");
    };
    let limit = limit.min(MAX_ROWS_PER_FETCH) as usize;
    let start = (offset as usize).min(hits.len());
    let end = (start + limit).min(hits.len());
    let rows = tokio::task::spawn_blocking(move || {
        let snap: Vec<Snapshot> = state.snapshot("result rows");
        let guards: Vec<_> = snap.iter().map(|s| s.index.read().unwrap()).collect();
        // Map the device order captured at search time onto the current snapshot; a device that is
        // gone or whose rows were renumbered (compaction, rescan) cannot serve its old hits.
        let order: Vec<Option<usize>> = devices
            .iter()
            .map(|(id, epoch)| {
                snap.iter()
                    .zip(&guards)
                    .position(|(s, g)| s.device.id == *id && g.epoch() == *epoch)
            })
            .collect();
        let devs: Vec<SearchDevice> = snap
            .iter()
            .zip(&guards)
            .map(|(s, g)| SearchDevice {
                device: &s.device,
                index: g,
                mount: &s.device.last_mount_point,
                online: s.online,
                pc: &s.pc,
            })
            .collect();
        // All or nothing: rows are positional on the client, so a page must never have gaps.
        hits[start..end]
            .iter()
            .map(|h| {
                let dev = (*order.get(h.dev as usize)?)?;
                Some(materialize(
                    &devs,
                    Hit {
                        dev: dev as u16,
                        row: h.row,
                    },
                ))
            })
            .collect::<Option<Vec<_>>>()
    })
    .await;
    match rows {
        Ok(Some(rows)) => Response::Rows {
            search_id,
            offset: start as u32,
            rows,
        },
        Ok(None) => err("results changed; search again"),
        Err(e) => err(e.to_string()),
    }
}

fn forget(state: &Arc<State>, id: DeviceId) -> Response {
    let Some(d) = state.device(id) else {
        return err("unknown drive");
    };
    if matches!(d.state, DeviceState::Online | DeviceState::Scanning) {
        return err("only offline drives can be forgotten");
    }
    state.registry.write().unwrap().remove(id);
    state.drop_index(id);
    state.dirty.lock().unwrap().remove(&id);
    let _ = std::fs::remove_file(state.index_path(id));
    state.remove_journal(id);
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
    if let Some(d) = state
        .registry
        .read()
        .unwrap()
        .find_own_by_identity(&identity)
    {
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
    let seen_by_service = state
        .mounted
        .lock()
        .unwrap()
        .values()
        .any(|m| m.identity == v.identity);
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
    let existing = state
        .registry
        .read()
        .unwrap()
        .find_own_by_identity(&v.identity)
        .map(|d| d.id);
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
        None => err("unknown drive"),
    }
}

fn push_share(state: &Arc<State>, id: DeviceId, data_b64: &str) -> Response {
    match state.device(id) {
        Some(d) if is_app_fed(state, &d) => {}
        Some(_) => return err("drive is indexed by the service"),
        None => return err("unknown drive"),
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
    state.replace_index(id, ix);
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
    let devices = state.visible_devices();
    let online = devices
        .iter()
        .filter(|d| matches!(d.state, DeviceState::Online | DeviceState::Scanning))
        .count();
    StatusInfo {
        service_version: env!("CARGO_PKG_VERSION").into(),
        total_entries: devices.iter().map(|d| d.entry_count).sum(),
        devices_online: online as u32,
        devices_offline: (devices.len() - online) as u32,
        indexing: state
            .scanning
            .lock()
            .unwrap()
            .iter()
            .map(|(id, n)| IndexProgress {
                device_id: *id,
                scanned: *n,
                done: false,
            })
            .collect(),
    }
}

#[cfg(test)]
mod same_size_tests {
    use super::*;

    #[test]
    fn a_page_is_capped_but_never_splits_a_group() {
        let sizes = [(9, 9_000), (8, 9_000), (7, 9_000)];
        assert_eq!(groups_in_page(&sizes, u32::MAX), 2, "18000 fits the cap, 27000 does not");
        assert_eq!(groups_in_page(&sizes, 9_000), 1);
        assert_eq!(groups_in_page(&sizes, 0), 1, "always one group");
        assert_eq!(groups_in_page(&[(5, 30_000), (4, 2)], u32::MAX), 1, "one group may exceed the cap");
        assert_eq!(groups_in_page(&[], 10), 0);
    }

    #[test]
    fn a_known_drive_without_an_index_is_reported_not_skipped() {
        let (a, b, c) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2), uuid::Uuid::from_u128(3));
        // c is not a drive at all: skipped; b is a drive whose index did not load
        assert_eq!(first_unloaded(&[a, b, c], &[a, b], &[a]), Some(b));
        assert_eq!(first_unloaded(&[a, c], &[a, b], &[a]), None);
        assert_eq!(first_unloaded(&[], &[a], &[]), None);
    }
}
