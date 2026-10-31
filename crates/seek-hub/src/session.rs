//! The hub's side of a session with one linked PC (spec §6). Strictly request → reply: the loop reads one
//! frame and writes the frames answering it, if any. Nothing here writes on a timer or of its own accord;
//! idleness is the listener's business (its stream fails the read here). Refusals are counted in `counters`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use seek_sync::noise::{SecureReader, SecureWriter};
use seek_sync::{Cursor, Error, Frame, Msg, Refusal, Role, SYNC_PROTOCOL};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::net::Counters;
use crate::store::{PutError, Served, Store};

/// A compaction decodes a whole copy: one at a time across every session of the hub.
static COMPACTING: Semaphore = Semaphore::const_new(1);

/// Largest JSON part of a frame from a PC.
const MAX_JSON: usize = 4 << 20;

/// The frames answering one message (JSON part, payload).
type Replies = Vec<(Msg, Vec<u8>)>;

/// Serves `pc` (already authenticated by its key) until it closes the connection or unlinks.
/// The first frame must be the PC's `Hello` for protocol v2 under its own host id; otherwise the session
/// ends with an error and nothing is written.
pub async fn serve_pc<R, W>(
    store: Arc<Store>,
    counters: &Counters,
    pc: Uuid,
    mut rd: SecureReader<R>,
    mut wr: SecureWriter<W>,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // an authenticated PC is still bounded: its quota, and a small JSON part
    rd.set_max_frame(store.max_frame());
    rd.set_max_json(MAX_JSON);
    let Some(first) = next(&mut rd).await? else { return Ok(()) };
    match first.msg()? {
        Msg::Hello { protocol, host_id, .. } => {
            if protocol != SYNC_PROTOCOL {
                return Err(Error::Protocol(format!("PC speaks sync protocol v{protocol}, the hub speaks v{SYNC_PROTOCOL}")));
            }
            if host_id != pc {
                return Err(Error::Protocol(format!("PC {pc} said hello as {host_id}")));
            }
        }
        _ => return Err(Error::Protocol("expected hello".into())),
    }
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: store.hub_id(), name: store.name(), role: Role::Hub };
    wr.send(&Frame::new(&hello, vec![])).await?;
    let s = store.clone();
    blocking(move || s.touch_pc(pc, now_ms())).await?;

    loop {
        let Some(frame) = next(&mut rd).await? else { return Ok(()) };
        let msg: Msg = frame.msg()?;
        match msg {
            Msg::Hello { .. } => return Err(Error::Protocol("unexpected hello".into())),
            Msg::Pull { cursors } => pull(&store, pc, cursors, &mut wr).await?,
            Msg::Unlinked => {
                let s = store.clone();
                match blocking(move || s.unlink_pc(pc)).await? {
                    Ok(_) => tracing::info!("PC {pc} unlinked itself"),
                    Err(e) => return Err(Error::Io(std::io::Error::other(format!("unlinking PC {pc}: {e}")))),
                }
                return Ok(());
            }
            msg => {
                let s = store.clone();
                let (replies, compact) = blocking(move || {
                    let r = answer(&s, pc, msg, frame.payload);
                    s.touch_pc(pc, now_ms());
                    r
                })
                .await?;
                for (m, payload) in replies {
                    if let Msg::Refused { reason } = &m {
                        counters.refusal(pc, reason);
                    }
                    wr.send(&Frame::new(&m, payload)).await?;
                }
                if let Some(device) = compact {
                    compact_later(store.clone(), device);
                }
            }
        }
    }
}

/// The whole answer to a tombstoned PC's handshake: one `Unlinked`, then the connection closes.
pub async fn reply_unlinked<W: AsyncWrite + Unpin>(mut wr: SecureWriter<W>) -> Result<(), Error> {
    wr.send(&Frame::new(&Msg::Unlinked, vec![])).await
}

/// The next frame; `None` when the PC closed the connection.
async fn next<R: AsyncRead + Unpin>(rd: &mut SecureReader<R>) -> Result<Option<Frame>, Error> {
    match rd.recv().await {
        Ok(f) => Ok(Some(f)),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Protocol(format!("worker failed: {e}")))
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn refused(reason: Refusal) -> Replies {
    vec![(Msg::Refused { reason }, vec![])]
}

/// Answers one message other than `Hello`, `Pull` and `Unlinked`; also names a device whose journal is now
/// due for compaction.
fn answer(store: &Store, pc: Uuid, msg: Msg, payload: Vec<u8>) -> (Replies, Option<Uuid>) {
    let replies = match msg {
        Msg::Devices { devices, profiles } => {
            let mut ids: Vec<Uuid> = devices.iter().map(|d| d.id).collect();
            ids.sort();
            ids.dedup();
            match store.set_pc_devices(pc, devices, profiles) {
                Ok(()) => {
                    // devices left out of `Have` are sent in full
                    let cursors = ids.into_iter().filter_map(|d| store.cursor(d).map(|c| (d, c))).collect();
                    vec![(Msg::Have { cursors }, vec![])]
                }
                Err(reason) => refused(reason),
            }
        }
        Msg::Full { device, cursor } => match store.put_full(pc, device, cursor, payload) {
            Ok(()) => vec![(Msg::Ack { device, cursor }, vec![])],
            Err(reason) => refused(reason),
        },
        Msg::Delta { device, from, to } => match store.put_delta(pc, device, from, to, payload) {
            Ok(()) => {
                let due = store.compact_due().contains(&device).then_some(device);
                return (vec![(Msg::Ack { device, cursor: to }, vec![])], due);
            }
            // the stored copy does not continue there: ask for a full one
            Err(PutError::Mismatch) => vec![(Msg::Have { cursors: vec![(device, Cursor::none())] }, vec![])],
            Err(PutError::Refused(reason)) => refused(reason),
        },
        // relay channels are not built
        Msg::Open { .. } => refused(Refusal::NotSupported),
        // Acks need no answer; a PC holds no hub-owned data (`Have`) and has nothing to ask with `SyncNow`
        // (the hub has nothing to push); the rest are hub-to-PC messages or unbuilt relay traffic.
        Msg::Ack { .. }
        | Msg::SyncNow
        | Msg::Have { .. }
        | Msg::MemberJoined { .. }
        | Msg::Data { .. }
        | Msg::Close { .. }
        | Msg::Catalog { .. }
        | Msg::PullDone
        | Msg::Refused { .. }
        | Msg::Hello { .. }
        | Msg::Pull { .. }
        | Msg::Unlinked => vec![],
    };
    (replies, None)
}

/// Answers `Pull`: the catalog, then what each listed device needs given the PC's cursor for it, then
/// `PullDone`. One device is read at a time, so at most one full copy is in memory.
async fn pull<W: AsyncWrite + Unpin>(
    store: &Arc<Store>,
    pc: Uuid,
    cursors: Vec<(Uuid, Cursor)>,
    wr: &mut SecureWriter<W>,
) -> Result<(), Error> {
    let s = store.clone();
    let (pcs, devices) = blocking(move || {
        s.touch_pc(pc, now_ms());
        s.catalog_for(pc)
    })
    .await?;
    let ids: Vec<Uuid> = devices.iter().map(|d| d.id).collect();
    wr.send(&Frame::new(&Msg::Catalog { pcs, devices }, vec![])).await?;
    for device in ids {
        let have = cursors.iter().find(|(d, _)| *d == device).map(|(_, c)| *c);
        let s = store.clone();
        for served in blocking(move || s.serve(device, have)).await? {
            let frame = match served {
                Served::Full { cursor, bytes } => Frame::new(&Msg::Full { device, cursor }, bytes),
                Served::Delta { from, to, payload } => Frame::new(&Msg::Delta { device, from, to }, payload),
            };
            wr.send(&frame).await?;
        }
    }
    wr.send(&Frame::new(&Msg::PullDone, vec![])).await
}

/// Folds the device's journal into its full copy in the background, without holding up the reply.
fn compact_later(store: Arc<Store>, device: Uuid) {
    tokio::spawn(async move {
        let Ok(_permit) = COMPACTING.acquire().await else { return };
        let done = blocking(move || {
            // queued behind another compaction of the same device, which may have done the job
            if !store.compact_due().contains(&device) {
                return Ok(());
            }
            store.compact(device)
        })
        .await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("compacting device {device} failed: {e}"),
            Err(e) => tracing::warn!("compacting device {device}: {e}"),
        }
    });
}

#[cfg(test)]
pub(crate) mod tests;
