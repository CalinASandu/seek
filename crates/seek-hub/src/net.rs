//! The hub's listener (spec §9): who gets in, and the limits on everyone before they do.
//!
//! Every connection starts with one byte from the dialer, as in LAN sync: `b'P'` = pairing, `b'N'` = session;
//! anything else is closed. Until a connection is authenticated (or its pairing attempt ends) it holds one of
//! 16 permits and must be done within 5 s of being accepted; at most 60 connections a minute get that far
//! (global: behind Funnel every client is localhost). A refused connection is closed at once. A stranger is
//! never written a byte: an unknown key and a pairing request with no window open are closed in silence.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use seek_sync::noise;
use seek_sync::pairing::{pair_host_then, PairingCode};
use seek_sync::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant, Sleep};
use uuid::Uuid;

use crate::config::HubConfig;
pub use crate::counters::{CounterValues, Counters, Event};
use crate::pairing::{self, PairOutcome, Pairing, PairingState};
use crate::session::{now_ms, reply_unlinked, serve_pc};
use crate::store::Store;

pub const PAIRING: u8 = b'P';
pub const SESSION: u8 = b'N';
/// Sent before SPAKE2 when a window is open (the LAN host's "ok").
const STATUS_OK: u8 = 1;
/// From accept to authenticated (or to the end of a pairing attempt).
const PRE_AUTH: Duration = Duration::from_secs(5);
const MAX_FIRST_MSG: usize = 256;
const MAX_UNAUTH: usize = 16;
const MAX_ATTEMPTS: usize = 60;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
/// A session ends after this long without a byte from the PC.
const IDLE: Duration = Duration::from_secs(3 * 60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- limits ----

/// Connection attempts of the last minute.
#[derive(Default)]
struct RateWindow {
    recent: VecDeque<Instant>,
}

impl RateWindow {
    /// Counts an attempt made at `now`; `false` (and not counted) when 60 were made in the minute before.
    fn hit(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) >= ATTEMPT_WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_ATTEMPTS {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// A stream whose reads and writes fail with `TimedOut` after [`IDLE`] without progress: no byte received
/// while a read was waiting, or none taken by the socket while a write (or flush) was waiting. Each wait
/// starts when the operation starts and over with every bit of progress, so time spent writing replies does
/// not count against the reads, and a PC that stops reading (or vanishes mid-pull) cannot hold a write forever.
struct Idle<S> {
    io: S,
    read: Stall,
    write: Stall,
}

impl<S> Idle<S> {
    fn new(io: S) -> Idle<S> {
        Idle { io, read: Stall::new("nothing received for 3 min"), write: Stall::new("nothing sent for 3 min") }
    }
}

/// The 3 min clock of one direction of an [`Idle`] stream.
struct Stall {
    sleep: Pin<Box<Sleep>>,
    waiting: bool,
    what: &'static str,
}

impl Stall {
    fn new(what: &'static str) -> Stall {
        Stall { sleep: Box::pin(tokio::time::sleep(IDLE)), waiting: false, what }
    }

    /// Polls `op`; fails it once it has been pending for [`IDLE`] since it started or last made progress.
    fn guard<T>(
        &mut self,
        cx: &mut Context<'_>,
        op: impl FnOnce(&mut Context<'_>) -> Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if !self.waiting {
            self.sleep.as_mut().reset(Instant::now() + IDLE);
            self.waiting = true;
        }
        match op(cx) {
            Poll::Ready(r) => {
                self.waiting = false;
                Poll::Ready(r)
            }
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.waiting = false;
                    Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, self.what)))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Idle<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = &mut *self;
        me.read.guard(cx, |cx| Pin::new(&mut me.io).poll_read(cx, buf))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Idle<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let me = &mut *self;
        me.write.guard(cx, |cx| Pin::new(&mut me.io).poll_write(cx, buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = &mut *self;
        me.write.guard(cx, |cx| Pin::new(&mut me.io).poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = &mut *self;
        me.write.guard(cx, |cx| Pin::new(&mut me.io).poll_shutdown(cx))
    }
}

// ---- the hub ----

pub struct Hub {
    store: Arc<Store>,
    cfg: HubConfig,
    /// Bound addresses, one per `cfg.listen` entry.
    addrs: Vec<SocketAddr>,
    pairing: Mutex<Pairing>,
    counters: Counters,
    unauth: Arc<Semaphore>,
    attempts: Mutex<RateWindow>,
    /// The running session of each PC; notified to end it (a newer one replaced it, the PC was unlinked, the
    /// key is being rotated).
    sessions: Mutex<HashMap<Uuid, Arc<Notify>>>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Hub {
    /// Binds every `cfg.listen` address and starts accepting.
    pub async fn start(cfg: HubConfig, store: Arc<Store>) -> Result<Arc<Hub>, Error> {
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for a in &cfg.listen {
            let l = TcpListener::bind(a).await?;
            let local = l.local_addr()?;
            tracing::info!("listening on {local}");
            addrs.push(local);
            listeners.push(l);
        }
        let hub = Arc::new(Hub {
            store,
            cfg,
            addrs,
            pairing: Mutex::new(Pairing::default()),
            counters: Counters::default(),
            unauth: Arc::new(Semaphore::new(MAX_UNAUTH)),
            attempts: Mutex::new(RateWindow::default()),
            sessions: Mutex::new(HashMap::new()),
            stop: watch::Sender::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let tasks = listeners.into_iter().map(|l| tokio::spawn(hub.clone().accept_loop(l))).collect();
        *lock(&hub.tasks) = tasks;
        Ok(hub)
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.clone()
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Stops listening and closes every connection.
    pub fn shutdown(&self) {
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
        self.stop.send_replace(true);
    }

    /// Opens a pairing window with a new 8-digit code, replacing any open one.
    pub fn open_pairing(&self, now_ms: i64) -> PairingCode {
        let code = lock(&self.pairing).open(now_ms);
        tracing::info!("pairing window open for 2 min");
        code
    }

    /// Whether a window is open with attempts left.
    pub fn pairing_open(&self) -> bool {
        lock(&self.pairing).is_open(now_ms())
    }

    /// The current (or last) window and how it ended: linked a PC, expired, or too many failed attempts.
    pub fn pairing_state(&self) -> PairingState {
        lock(&self.pairing).state(now_ms(), PRE_AUTH.as_millis() as i64)
    }

    /// How the last window ended; `None` while it is open (or none was opened).
    pub fn pairing_outcome(&self) -> Option<PairOutcome> {
        self.pairing_state().outcome
    }

    /// Ends `pc`'s session, if it has one (it was just unlinked).
    pub fn close_session(&self, pc: Uuid) {
        if let Some(s) = lock(&self.sessions).get(&pc) {
            s.notify_one();
        }
    }

    /// Ends every session (before a key rotation); the PCs reconnect on their own.
    pub fn close_sessions(&self) {
        for s in lock(&self.sessions).values() {
            s.notify_one();
        }
    }

    /// Connections not yet authenticated.
    #[cfg(test)]
    fn unauthenticated(&self) -> usize {
        MAX_UNAUTH - self.unauth.available_permits()
    }

    #[cfg(test)]
    fn has_session(&self, pc: Uuid) -> bool {
        lock(&self.sessions).contains_key(&pc)
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((io, _)) => {
                    // a refused connection is dropped right here, before anything is read
                    let Some(permit) = self.admit() else { continue };
                    let hub = self.clone();
                    tokio::spawn(async move { hub.connection(io, permit).await });
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    /// A permit for a new connection, if the pre-auth limits allow one.
    fn admit(&self) -> Option<OwnedSemaphorePermit> {
        let Ok(permit) = self.unauth.clone().try_acquire_owned() else {
            self.counters.record(Event::RefusedHandshake, "16 connections are already handshaking");
            return None;
        };
        if !lock(&self.attempts).hit(Instant::now()) {
            self.counters.record(Event::RefusedHandshake, "over 60 connections in a minute");
            return None;
        }
        Some(permit)
    }

    async fn connection(self: Arc<Self>, io: TcpStream, permit: OwnedSemaphorePermit) {
        let _ = io.set_nodelay(true);
        let mut stop = self.stop.subscribe();
        tokio::select! {
            _ = self.handle(Idle::new(io), permit) => {}
            _ = stop.wait_for(|s| *s) => {}
        }
    }

    async fn handle(&self, mut io: Idle<TcpStream>, permit: OwnedSemaphorePermit) {
        let deadline = Instant::now() + PRE_AUTH;
        let mut kind = [0u8; 1];
        match timeout_at(deadline, io.read_exact(&mut kind)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                self.counters.record(Event::RefusedHandshake, "closed before the handshake");
                return;
            }
            Err(_) => {
                self.counters.record(Event::RefusedHandshake, "no handshake within 5 s");
                return;
            }
        }
        match kind[0] {
            SESSION => self.session(io, permit, deadline).await,
            PAIRING => self.pair(io, permit, deadline).await,
            _ => {
                self.counters.record(Event::RefusedHandshake, "unknown connection kind");
            }
        }
    }

    async fn session(&self, io: Idle<TcpStream>, permit: OwnedSemaphorePermit, deadline: Instant) {
        let keys = self.store.noise_keys();
        let store = &self.store;
        // never the store's lock: a stranger must not wait behind a long write (or hold up a worker)
        let known = |k: &[u8; 32]| store.is_known_key(k);
        let (theirs, rd, wr) = match timeout_at(deadline, noise::accept_limited(io, &keys, MAX_FIRST_MSG, known)).await {
            Ok(Ok(x)) => x,
            Ok(Err(Error::NotLinked)) => {
                self.counters.record(Event::RefusedHandshake, "unknown key");
                return;
            }
            Ok(Err(e)) => {
                self.counters.record(Event::RefusedHandshake, format_args!("handshake failed: {e}"));
                return;
            }
            Err(_) => {
                self.counters.record(Event::RefusedHandshake, "no handshake within 5 s");
                return;
            }
        };
        drop(permit);

        let store = self.store.clone();
        let found = tokio::task::spawn_blocking(move || store.pc_by_key(&theirs)).await.ok().flatten();
        let Some(rec) = found else {
            // tombstoned (or unlinked a moment ago): the whole answer, then the connection closes
            if let Err(e) = reply_unlinked(wr).await {
                tracing::debug!("telling an unlinked PC so failed: {e}");
            }
            tracing::info!("an unlinked PC connected; told it so");
            return;
        };
        let (pc, name) = (rec.host_id, rec.name);
        let stop = Arc::new(Notify::new());
        if let Some(old) = lock(&self.sessions).insert(pc, stop.clone()) {
            old.notify_one();
        }
        tracing::info!("{name} connected");
        tokio::select! {
            r = serve_pc(self.store.clone(), &self.counters, pc, rd, wr) => match r {
                Ok(()) => tracing::info!("{name} disconnected"),
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut => {
                    tracing::info!("{name} made no progress for 3 min ({e}); session closed")
                }
                Err(e) => tracing::info!("session with {name} ended: {e}"),
            },
            _ = stop.notified() => tracing::info!("session with {name} closed (replaced, unlinked or key rotation)"),
        }
        let mut sessions = lock(&self.sessions);
        if sessions.get(&pc).is_some_and(|s| Arc::ptr_eq(s, &stop)) {
            sessions.remove(&pc);
        }
    }

    async fn pair(&self, mut io: Idle<TcpStream>, permit: OwnedSemaphorePermit, deadline: Instant) {
        let attempt = lock(&self.pairing).take_attempt(now_ms());
        let Some((code, seq)) = attempt else {
            self.counters.record(Event::PairingFailure, "pairing asked for with no window open");
            return;
        };
        let mine = pairing::hub_info(&self.store, pairing::hub_addrs(&self.cfg, &self.addrs));
        // The guest is saved (and the window closed) BEFORE the hub's reply goes out, so a PC that dials the
        // moment it sees the reply is already known.
        let result: Mutex<Option<PairOutcome>> = Mutex::new(None);
        let accept = |guest: seek_sync::pairing::PairInfo| async {
            let rec = match pairing::pc_record(guest, now_ms()) {
                Ok(r) => r,
                Err(e) => {
                    self.counters.record(Event::PairingFailure, &e);
                    *lock(&result) = Some(PairOutcome::Failed { reason: e.clone() });
                    return Err(Error::Protocol(e));
                }
            };
            let name = rec.name.clone();
            let store = self.store.clone();
            let saved = tokio::task::spawn_blocking(move || store.add_pc(rec)).await;
            let failure = match saved {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(why) = failure {
                tracing::warn!("linking {name} failed: {why}");
                let reason = format!("saving {name} failed: {why}");
                *lock(&result) = Some(PairOutcome::Failed { reason: reason.clone() });
                return Err(Error::Protocol(reason));
            }
            // a success closes the window, unless an admin opened a new one meanwhile
            lock(&self.pairing).paired(seq);
            tracing::info!("linked {name}");
            *lock(&result) = Some(PairOutcome::Linked { name });
            Ok(())
        };
        let attempt = async {
            io.write_all(&[STATUS_OK]).await?;
            pair_host_then(&mut io, &code, &mine, accept).await
        };
        let ended = timeout_at(deadline, attempt).await;
        let outcome = lock(&result).take();
        match (ended, outcome) {
            (Ok(Ok(_)), Some(o)) => {
                drop(permit);
                lock(&self.pairing).linked(seq, o);
            }
            // rejected after the guest proved the code: it is not a wrong guess
            (_, Some(o)) => lock(&self.pairing).linked(seq, o),
            (Ok(Ok(_)), None) => {}
            (Ok(Err(e)), None) => {
                self.counters.record(Event::PairingFailure, format_args!("pairing attempt failed: {e}"));
                lock(&self.pairing).failed(seq);
            }
            (Err(_), None) => {
                self.counters.record(Event::PairingFailure, "pairing not finished within 5 s");
                lock(&self.pairing).failed(seq);
            }
        }
    }
}

#[cfg(test)]
mod tests;
