//! One sync session with a linked PC, independent of the transport and of the service.
//!
//! Each side is the owner of its own devices and sends only what the other side's copies miss: a delta of
//! journaled changes, or a full copy when the history does not reach back far enough. The service plugs in
//! through [`SyncHost`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use seek_core::{Change, Device};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};

use crate::frame::Frame;
use crate::journal::Cursor;
use crate::noise::{SecureReader, SecureWriter};
use crate::pairing::MemberInfo;
use crate::wire::{CatalogPc, Msg, Refusal, Role, SYNC_PROTOCOL};
use crate::Error;

/// What an owner sends so a copy catches up.
pub enum Outgoing {
    Delta { from: Cursor, to: Cursor, changes: Vec<Change> },
    Full { cursor: Cursor, bytes: Vec<u8> },
}

/// The service side of a session. Methods may block (they run on blocking threads).
pub trait SyncHost: Send + Sync + 'static {
    fn host_id(&self) -> uuid::Uuid;
    fn name(&self) -> String;
    /// Own devices shared with linked PCs (respects the sharing switch and per-device opt-out).
    fn shared_devices(&self) -> Vec<Device>;
    /// This PC's `(SID, profile folder)` list.
    fn profiles(&self) -> Vec<(String, String)>;
    /// What brings a copy at `have` up to date; `None` = up to date, or not an own shared device.
    fn outgoing(&self, device: uuid::Uuid, have: Option<Cursor>) -> Option<Outgoing>;
    /// Cursors of `owner`'s devices held here.
    fn held(&self, owner: uuid::Uuid) -> Vec<(uuid::Uuid, Cursor)>;
    /// Cursors of every copy of another PC's device held here (what a hub `Pull` reports).
    fn held_remote(&self) -> Vec<(uuid::Uuid, Cursor)>;
    /// `owner`'s current shared devices: add/update records, drop copies of devices no longer listed.
    fn remote_devices(&self, owner: uuid::Uuid, devices: Vec<Device>, profiles: Vec<(String, String)>);
    /// A hub's catalog: for every PC in `pcs`, the same as `remote_devices(that PC, its devices, its profiles)`;
    /// copies of owners received through the hub that are no longer in `pcs` are dropped.
    fn catalog(&self, hub: uuid::Uuid, pcs: Vec<CatalogPc>, devices: Vec<Device>);
    fn apply_delta(
        &self,
        owner: uuid::Uuid,
        device: uuid::Uuid,
        from: Cursor,
        to: Cursor,
        changes: Vec<Change>,
    ) -> Result<(), String>;
    fn apply_full(&self, owner: uuid::Uuid, device: uuid::Uuid, cursor: Cursor, bytes: Vec<u8>) -> Result<(), String>;
    /// Something was received from `peer` (records the last sync time).
    fn synced(&self, peer: uuid::Uuid);
    fn member_joined(&self, member: MemberInfo);
    /// `peer` removed the link.
    fn unlinked_by(&self, peer: uuid::Uuid);
    /// The hub did not take or serve something.
    fn hub_refused(&self, reason: Refusal) {
        tracing::warn!("the hub refused a request: {reason:?}");
    }
}

/// Handle to steer a running session.
pub struct SessionCtl {
    /// Push own changes now and ask the peer to do the same.
    pub sync_now: Notify,
    /// An own index changed (fired debounced by the service); a hub session uploads and pulls. LAN sessions ignore it.
    pub changed: Notify,
    /// End the session.
    pub stop: Notify,
    outbox: mpsc::UnboundedSender<Msg>,
}

impl SessionCtl {
    pub fn new() -> (Arc<SessionCtl>, mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctl = SessionCtl { sync_now: Notify::new(), changed: Notify::new(), stop: Notify::new(), outbox: tx };
        (Arc::new(ctl), rx)
    }

    /// Queues a message for the peer (e.g. `Unlinked`, `MemberJoined`). Ends the session after `Unlinked`.
    pub fn send(&self, m: Msg) {
        let _ = self.outbox.send(m);
    }
}

pub fn encode_delta(changes: &[Change]) -> Vec<u8> {
    let raw = postcard::to_stdvec(changes).expect("changes serialize");
    zstd::encode_all(raw.as_slice(), 1).expect("zstd in memory")
}

/// Most a delta may decompress to (a compressed one can claim far more than it weighs).
pub const MAX_DELTA_PLAIN: u64 = 512 << 20;

pub fn decode_delta(b: &[u8]) -> Result<Vec<Change>, Error> {
    decode_delta_limited(b, MAX_DELTA_PLAIN)
}

/// [`decode_delta`] refusing a delta that decompresses to more than `limit` bytes.
pub fn decode_delta_limited(b: &[u8], limit: u64) -> Result<Vec<Change>, Error> {
    use std::io::Read;
    let bad = |e: String| Error::Protocol(format!("bad delta: {e}"));
    let dec = zstd::Decoder::new(b).map_err(|e| bad(e.to_string()))?;
    let mut raw = Vec::new();
    dec.take(limit + 1).read_to_end(&mut raw).map_err(|e| bad(e.to_string()))?;
    if raw.len() as u64 > limit {
        return Err(bad(format!("larger than {limit} bytes unpacked")));
    }
    postcard::from_bytes(&raw).map_err(|e| bad(e.to_string()))
}

/// The owner side of a session: what the peer holds of our own devices.
pub(crate) struct Owner {
    /// What the peer's copy of each own device is at (as far as we know).
    pub(crate) sent: HashMap<uuid::Uuid, Cursor>,
    /// The peer told us what it has, so pushing makes sense.
    pub(crate) ready: bool,
    /// JSON of the device list last sent.
    last_devices: Vec<u8>,
    /// Ids of the own devices last listed.
    pub(crate) shared: HashSet<uuid::Uuid>,
    /// `Devices` messages sent that the peer has not answered yet (a hub answers each with `Have` or `Refused`).
    pub(crate) devices_pending: u32,
}

impl Owner {
    pub(crate) fn new() -> Owner {
        Owner { sent: HashMap::new(), ready: false, last_devices: Vec::new(), shared: HashSet::new(), devices_pending: 0 }
    }

    /// Notes what the peer holds of `device`. An answer from before the last update was sent (a `Have` for an
    /// earlier `Devices`, an `Ack` of an earlier delta) is behind `sent`: the answer to the update follows, and
    /// acting on the older one would send that update twice, which the peer takes for a gap and answers with a
    /// request for a full copy. A cursor of another generation, or none, always wins.
    pub(crate) fn peer_has(&mut self, device: uuid::Uuid, cursor: Cursor) {
        match self.sent.get(&device) {
            Some(known) if known.generation == cursor.generation && cursor.seq < known.seq => {}
            _ => {
                self.sent.insert(device, cursor);
            }
        }
    }
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Protocol(format!("worker failed: {e}")))
}

/// The frame channels of a session: a writer task drains `tx`, a reader task fills `rx`.
pub(crate) struct Pipes {
    pub(crate) tx: mpsc::Sender<Frame>,
    pub(crate) rx: mpsc::Receiver<Result<Frame, Error>>,
    writer: AbortOnDrop<Result<(), Error>>,
    _reader: AbortOnDrop<()>,
}

/// A task that ends with its owner, so a session that is aborted mid-way leaks nothing.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Pipes {
    pub(crate) fn spawn<R, W>(mut rd: SecureReader<R>, mut wr: SecureWriter<W>) -> Pipes
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut out_rx) = mpsc::channel::<Frame>(8);
        let writer = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                wr.send(&f).await?;
            }
            Ok::<_, Error>(())
        });
        let (in_tx, rx) = mpsc::channel::<Result<Frame, Error>>(8);
        let reader = tokio::spawn(async move {
            loop {
                let r = rd.recv().await;
                let end = r.is_err();
                if in_tx.send(r).await.is_err() || end {
                    break;
                }
            }
        });
        Pipes { tx, rx, writer: AbortOnDrop(writer), _reader: AbortOnDrop(reader) }
    }

    /// Lets queued frames go out (for up to 5 s), then stops reading.
    pub(crate) async fn close(mut self) {
        drop(self.tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.writer.0).await;
        // both tasks are aborted when `self` drops
    }
}

/// A received frame; `None` when the peer closed the connection.
pub(crate) fn incoming(r: Option<Result<Frame, Error>>) -> Result<Option<Frame>, Error> {
    match r {
        None => Ok(None),
        Some(Err(Error::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Some(Err(e)) => Err(e),
        Some(Ok(f)) => Ok(Some(f)),
    }
}

/// Runs a session until the connection ends, `ctl.stop` fires, or either side unlinks.
pub async fn run_session<R, W>(
    host: Arc<dyn SyncHost>,
    peer: uuid::Uuid,
    rd: SecureReader<R>,
    wr: SecureWriter<W>,
    ctl: Arc<SessionCtl>,
    mut outbox: mpsc::UnboundedReceiver<Msg>,
    interval: Duration,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut pipes = Pipes::spawn(rd, wr);
    let result = drive(host, peer, &ctl, &mut outbox, &pipes.tx, &mut pipes.rx, interval).await;
    pipes.close().await;
    result
}

pub(crate) async fn send(tx: &mpsc::Sender<Frame>, m: &Msg, payload: Vec<u8>) -> Result<(), Error> {
    tx.send(Frame::new(m, payload)).await.map_err(|_| Error::Protocol("connection closed".into()))
}

async fn drive(
    host: Arc<dyn SyncHost>,
    peer: uuid::Uuid,
    ctl: &SessionCtl,
    outbox: &mut mpsc::UnboundedReceiver<Msg>,
    tx: &mpsc::Sender<Frame>,
    rx: &mut mpsc::Receiver<Result<Frame, Error>>,
    interval: Duration,
) -> Result<(), Error> {
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: host.host_id(), name: host.name(), role: Role::Pc };
    send(tx, &hello, vec![]).await?;
    let mut owner = Owner::new();
    send_devices(&host, &mut owner, tx).await?;
    let h = host.clone();
    let held = blocking(move || h.held(peer)).await?;
    send(tx, &Msg::Have { cursors: held }, vec![]).await?;

    let mut hello = false;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = rx.recv() => {
                let Some(frame) = incoming(r)? else { return Ok(()) };
                let msg: Msg = frame.msg()?;
                if !hello {
                    match msg {
                        Msg::Hello { protocol, host_id, role, .. } => {
                            if protocol != SYNC_PROTOCOL {
                                return Err(Error::Protocol(format!("peer speaks sync protocol v{protocol}, we speak v{SYNC_PROTOCOL}")));
                            }
                            if role == Role::Hub {
                                return Err(Error::Protocol("that address is a hub - use Link a hub".into()));
                            }
                            if host_id != peer {
                                return Err(Error::Protocol("peer identified as another PC".into()));
                            }
                            hello = true;
                            continue;
                        }
                        _ => return Err(Error::Protocol("expected hello".into())),
                    }
                }
                if handle(&host, peer, &mut owner, tx, msg, frame.payload).await? {
                    return Ok(());
                }
            }
            _ = tick.tick() => push(&host, &mut owner, tx).await?,
            _ = ctl.sync_now.notified() => {
                send(tx, &Msg::SyncNow, vec![]).await?;
                push(&host, &mut owner, tx).await?;
            }
            _ = ctl.stop.notified() => return Ok(()),
            Some(m) = outbox.recv() => {
                let unlinked = m == Msg::Unlinked;
                send(tx, &m, vec![]).await?;
                if unlinked {
                    return Ok(());
                }
            }
        }
    }
}

/// Handles one message; `true` ends the session.
async fn handle(
    host: &Arc<dyn SyncHost>,
    peer: uuid::Uuid,
    owner: &mut Owner,
    tx: &mpsc::Sender<Frame>,
    msg: Msg,
    payload: Vec<u8>,
) -> Result<bool, Error> {
    match msg {
        Msg::Hello { .. } => return Err(Error::Protocol("unexpected hello".into())),
        Msg::Devices { devices, profiles } => {
            let h = host.clone();
            blocking(move || {
                h.remote_devices(peer, devices, profiles);
                h.synced(peer);
            })
            .await?;
        }
        Msg::Have { cursors } => {
            for (device, cursor) in cursors {
                owner.peer_has(device, cursor);
            }
            owner.ready = true;
            push(host, owner, tx).await?;
        }
        Msg::Delta { device, from, to } => {
            let h = host.clone();
            let applied = blocking(move || {
                let r = decode_delta(&payload)
                    .map_err(|e| e.to_string())
                    .and_then(|changes| h.apply_delta(peer, device, from, to, changes));
                h.synced(peer);
                r
            })
            .await?;
            answer(device, to, applied, tx).await?;
        }
        Msg::Full { device, cursor } => {
            let h = host.clone();
            let applied = blocking(move || {
                let r = h.apply_full(peer, device, cursor, payload);
                h.synced(peer);
                r
            })
            .await?;
            answer(device, cursor, applied, tx).await?;
        }
        Msg::Ack { device, cursor } => owner.peer_has(device, cursor),
        Msg::SyncNow => push(host, owner, tx).await?,
        Msg::Unlinked => {
            let h = host.clone();
            blocking(move || h.unlinked_by(peer)).await?;
            return Ok(true);
        }
        Msg::MemberJoined { member } => {
            let h = host.clone();
            blocking(move || h.member_joined(member)).await?;
        }
        // Hub-only messages mean nothing between two PCs.
        Msg::Pull { .. } | Msg::Catalog { .. } | Msg::PullDone | Msg::Refused { .. } | Msg::Data { .. } | Msg::Close { .. } => {}
        Msg::Open { .. } => send(tx, &Msg::Refused { reason: Refusal::NotSupported }, vec![]).await?,
    }
    Ok(false)
}

/// Acknowledges an applied update, or asks for a full copy when it could not be applied.
async fn answer(device: uuid::Uuid, cursor: Cursor, applied: Result<(), String>, tx: &mpsc::Sender<Frame>) -> Result<(), Error> {
    match applied {
        Ok(()) => send(tx, &Msg::Ack { device, cursor }, vec![]).await,
        Err(e) => {
            tracing::warn!("sync update for device {device} not applied ({e}); asking for a full copy");
            send(tx, &Msg::Have { cursors: vec![(device, Cursor::none())] }, vec![]).await
        }
    }
}

pub(crate) async fn send_devices(host: &Arc<dyn SyncHost>, owner: &mut Owner, tx: &mpsc::Sender<Frame>) -> Result<Vec<Device>, Error> {
    let h = host.clone();
    let (devices, profiles) = blocking(move || (h.shared_devices(), h.profiles())).await?;
    owner.shared = devices.iter().map(|d| d.id).collect();
    let json = serde_json::to_vec(&devices).unwrap_or_default();
    if json != owner.last_devices {
        send(tx, &Msg::Devices { devices: devices.clone(), profiles }, vec![]).await?;
        owner.last_devices = json;
        owner.devices_pending += 1;
    }
    Ok(devices)
}

/// Sends the device list if it changed, then whatever each copy on the peer misses.
pub(crate) async fn push(host: &Arc<dyn SyncHost>, owner: &mut Owner, tx: &mpsc::Sender<Frame>) -> Result<(), Error> {
    let devices = send_devices(host, owner, tx).await?;
    if !owner.ready {
        return Ok(());
    }
    owner.sent.retain(|id, _| devices.iter().any(|d| d.id == *id));
    for d in devices {
        let have = owner.sent.get(&d.id).copied();
        let h = host.clone();
        let id = d.id;
        let out = blocking(move || h.outgoing(id, have)).await?;
        match out {
            None => {}
            Some(Outgoing::Delta { from, to, changes }) => {
                let payload = blocking(move || encode_delta(&changes)).await?;
                send(tx, &Msg::Delta { device: id, from, to }, payload).await?;
                owner.sent.insert(id, to);
            }
            Some(Outgoing::Full { cursor, bytes }) => {
                send(tx, &Msg::Full { device: id, cursor }, bytes).await?;
                owner.sent.insert(id, cursor);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
