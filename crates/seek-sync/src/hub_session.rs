//! A PC's session with a hub. The PC is the owner of its own devices and uploads them as to a LAN peer; for
//! every other PC's devices the hub is the source, and the PC polls it with `Pull`. The hub only ever answers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::frame::Frame;
use crate::journal::Cursor;
use crate::noise::{SecureReader, SecureWriter};
use crate::session::{blocking, decode_delta, incoming, push, send, Owner, Pipes, SessionCtl, SyncHost};
use crate::wire::{Msg, Role, SYNC_PROTOCOL};
use crate::Error;

/// Where the session is with polling the hub.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pulling {
    /// The first upload is not done yet.
    NotYet,
    /// A `Pull` awaits its `PullDone`.
    InFlight,
    Idle,
}

/// What the session knows: our side as an owner, and who owns each device of the hub's last catalog.
struct Hub {
    id: uuid::Uuid,
    owner: Owner,
    owners: HashMap<uuid::Uuid, uuid::Uuid>,
    pulling: Pulling,
    /// Copies whose last update could not be applied: the next `Pull` asks for them in full.
    refetch: HashSet<uuid::Uuid>,
}

/// Runs a session with `hub` until the connection ends, `ctl.stop` fires, or either side unlinks.
pub async fn run_hub_session<R, W>(
    host: Arc<dyn SyncHost>,
    hub: uuid::Uuid,
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
    let result = drive(host, hub, &ctl, &mut outbox, &pipes.tx, &mut pipes.rx, interval).await;
    pipes.close().await;
    result
}

async fn drive(
    host: Arc<dyn SyncHost>,
    hub: uuid::Uuid,
    ctl: &SessionCtl,
    outbox: &mut mpsc::UnboundedReceiver<Msg>,
    tx: &mpsc::Sender<Frame>,
    rx: &mut mpsc::Receiver<Result<Frame, Error>>,
    interval: Duration,
) -> Result<(), Error> {
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: host.host_id(), name: host.name(), role: Role::Pc };
    send(tx, &hello, vec![]).await?;
    let first = tokio::select! {
        r = rx.recv() => r,
        _ = ctl.stop.notified() => return Ok(()),
    };
    let Some(frame) = incoming(first)? else { return Ok(()) };
    match frame.msg()? {
        // the hub unlinked this PC: its whole answer is this one frame, without a hello
        Msg::Unlinked => {
            let h = host.clone();
            blocking(move || h.unlinked_by(hub)).await?;
            return Ok(());
        }
        Msg::Hello { protocol, host_id, role, .. } => {
            if protocol != SYNC_PROTOCOL {
                return Err(Error::Protocol(format!("hub speaks sync protocol v{protocol}, we speak v{SYNC_PROTOCOL}")));
            }
            if role != Role::Hub {
                return Err(Error::Protocol("that address is a PC, not a hub".into()));
            }
            if host_id != hub {
                return Err(Error::Protocol("peer identified as another hub".into()));
            }
        }
        _ => return Err(Error::Protocol("expected hello".into())),
    }

    let mut s =
        Hub { id: hub, owner: Owner::new(), owners: HashMap::new(), pulling: Pulling::NotYet, refetch: HashSet::new() };
    // not ready yet: only sends the device list; the hub answers with `Have`
    push(&host, &mut s.owner, tx).await?;

    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = rx.recv() => {
                let Some(frame) = incoming(r)? else { return Ok(()) };
                let msg: Msg = frame.msg()?;
                if handle(&host, &mut s, tx, msg, frame.payload).await? {
                    return Ok(());
                }
            }
            _ = tick.tick() => poll(&host, &mut s, tx).await?,
            _ = ctl.sync_now.notified() => poll(&host, &mut s, tx).await?,
            _ = ctl.changed.notified() => poll(&host, &mut s, tx).await?,
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

/// Asks the hub for the other PCs' devices, reporting the copies held here.
async fn pull(host: &Arc<dyn SyncHost>, s: &mut Hub, tx: &mpsc::Sender<Frame>) -> Result<(), Error> {
    let h = host.clone();
    let mut cursors = blocking(move || h.held_remote()).await?;
    cursors.retain(|(id, _)| !s.owner.shared.contains(id) && !s.refetch.contains(id));
    // a copy that could not be applied is reported as missing, so the hub serves it in full
    cursors.extend(s.refetch.iter().map(|id| (*id, Cursor::none())));
    send(tx, &Msg::Pull { cursors }, vec![]).await?;
    s.pulling = Pulling::InFlight;
    Ok(())
}

/// A tick, *Sync now* or an index change: upload own changes, then pull unless a pull is still being answered.
async fn poll(host: &Arc<dyn SyncHost>, s: &mut Hub, tx: &mpsc::Sender<Frame>) -> Result<(), Error> {
    push(host, &mut s.owner, tx).await?;
    if s.pulling == Pulling::Idle {
        pull(host, s, tx).await?;
    }
    Ok(())
}

/// The owner of a received copy, or `None` (with a warning) when the hub has no business sending it.
fn owner_of(s: &Hub, device: uuid::Uuid) -> Option<uuid::Uuid> {
    if s.owner.shared.contains(&device) {
        tracing::warn!("the hub sent data for own device {device}; ignored");
        return None;
    }
    let owner = s.owners.get(&device).copied();
    if owner.is_none() {
        tracing::warn!("the hub sent data for device {device}, which is not in its catalog; ignored");
    }
    owner
}

/// Acknowledges an applied copy. A copy that could not be applied gets no answer (the hub does not take `Have`
/// from a PC): the next `Pull` reports it as missing instead.
async fn applied_or_refetch(
    s: &mut Hub,
    device: uuid::Uuid,
    cursor: Cursor,
    applied: Result<(), String>,
    tx: &mpsc::Sender<Frame>,
) -> Result<(), Error> {
    match applied {
        Ok(()) => {
            s.refetch.remove(&device);
            send(tx, &Msg::Ack { device, cursor }, vec![]).await
        }
        Err(e) => {
            tracing::warn!("hub copy of device {device} not applied ({e}); asking for a full copy at the next pull");
            s.refetch.insert(device);
            Ok(())
        }
    }
}

/// Handles one message from the hub; `true` ends the session.
async fn handle(host: &Arc<dyn SyncHost>, s: &mut Hub, tx: &mpsc::Sender<Frame>, msg: Msg, payload: Vec<u8>) -> Result<bool, Error> {
    let hub = s.id;
    match msg {
        Msg::Hello { .. } => return Err(Error::Protocol("unexpected hello".into())),
        Msg::Have { cursors } => {
            s.owner.devices_pending = s.owner.devices_pending.saturating_sub(1);
            for (device, cursor) in cursors {
                s.owner.peer_has(device, cursor);
            }
            s.owner.ready = true;
            push(host, &mut s.owner, tx).await?;
            if s.pulling == Pulling::NotYet {
                pull(host, s, tx).await?;
            }
        }
        Msg::Ack { device, cursor } => s.owner.peer_has(device, cursor),
        Msg::Catalog { pcs, devices } => {
            // never let the hub speak for this PC: its own record and devices stay out of what the host sees
            let me = host.host_id();
            let pcs: Vec<_> = pcs.into_iter().filter(|pc| pc.host_id != me).collect();
            let listed: HashSet<uuid::Uuid> = pcs.iter().map(|pc| pc.host_id).collect();
            let devices: Vec<_> = devices
                .into_iter()
                .filter(|d| listed.contains(&d.indexed_by_host) && !s.owner.shared.contains(&d.id))
                .collect();
            s.owners = devices.iter().map(|d| (d.id, d.indexed_by_host)).collect();
            s.refetch.retain(|id| s.owners.contains_key(id));
            let h = host.clone();
            blocking(move || {
                h.catalog(hub, pcs, devices);
                h.synced(hub);
            })
            .await?;
        }
        Msg::Delta { device, from, to } => {
            let Some(pc) = owner_of(s, device) else { return Ok(false) };
            let h = host.clone();
            let applied = blocking(move || {
                let r = decode_delta(&payload)
                    .map_err(|e| e.to_string())
                    .and_then(|changes| h.apply_delta(pc, device, from, to, changes));
                h.synced(hub);
                r
            })
            .await?;
            applied_or_refetch(s, device, to, applied, tx).await?;
        }
        Msg::Full { device, cursor } => {
            let Some(pc) = owner_of(s, device) else { return Ok(false) };
            let h = host.clone();
            let applied = blocking(move || {
                let r = h.apply_full(pc, device, cursor, payload);
                h.synced(hub);
                r
            })
            .await?;
            applied_or_refetch(s, device, cursor, applied, tx).await?;
        }
        Msg::PullDone => {
            if s.pulling == Pulling::InFlight {
                s.pulling = Pulling::Idle;
            }
        }
        Msg::Refused { reason } => {
            if s.owner.devices_pending > 0 {
                // The hub does not take this PC's devices: uploads stay off until a `Have` says otherwise, but
                // downloads keep working. No `Have` follows, so the first pull must not wait for one.
                s.owner.devices_pending -= 1;
                s.owner.ready = false;
                if s.pulling == Pulling::NotYet {
                    pull(host, s, tx).await?;
                }
            } else if s.pulling == Pulling::InFlight {
                // the hub may refuse a pull; it will not send `PullDone` then, so the next poll may pull again
                s.pulling = Pulling::Idle;
            }
            // any other refusal (e.g. one upload over quota) leaves uploads on; the status line shows it
            let h = host.clone();
            blocking(move || h.hub_refused(reason)).await?;
        }
        Msg::Unlinked => {
            let h = host.clone();
            blocking(move || h.unlinked_by(hub)).await?;
            return Ok(true);
        }
        // A hub has no devices of its own and never asks for anything; relay channels are not built.
        Msg::Devices { .. }
        | Msg::SyncNow
        | Msg::MemberJoined { .. }
        | Msg::Pull { .. }
        | Msg::Open { .. }
        | Msg::Data { .. }
        | Msg::Close { .. } => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
