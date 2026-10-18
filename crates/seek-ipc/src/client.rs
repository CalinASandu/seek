//! Client side of the pipe protocol, used by the app and the CLI.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use tokio::io::WriteHalf;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::oneshot;

use crate::frame::{read_frame, write_frame};
use crate::protocol::{Envelope, Event, Request, Response, PIPE_NAME};

const ERROR_PIPE_BUSY: i32 = 231;

pub struct Client {
    pipe: NamedPipeClient,
    next_id: u64,
}

impl Client {
    pub async fn connect() -> io::Result<Client> {
        Self::connect_to(PIPE_NAME).await
    }

    pub async fn connect_to(name: &str) -> io::Result<Client> {
        // All server instances may be momentarily busy; retry briefly.
        for _ in 0..100 {
            match ClientOptions::new().open(name) {
                Ok(pipe) => return Ok(Client { pipe, next_id: 1 }),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "seek service pipe busy"))
    }

    /// Sends one request and waits for its response (pushed events are skipped).
    pub async fn request(&mut self, req: Request) -> io::Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.pipe, &Envelope { id, body: req }).await?;
        loop {
            let Some(env): Option<Envelope<Response>> = read_frame(&mut self.pipe).await? else {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "service closed the connection"));
            };
            if env.id == id {
                return Ok(env.body);
            }
        }
    }

    /// Turns this connection into an event stream. The stream ends when the service disconnects.
    pub async fn subscribe(mut self) -> io::Result<impl Stream<Item = Event>> {
        let id = self.next_id;
        write_frame(&mut self.pipe, &Envelope { id, body: Request::Subscribe }).await?;
        Ok(futures::stream::unfold(self.pipe, |mut pipe| async move {
            loop {
                match read_frame::<_, Envelope<Response>>(&mut pipe).await {
                    Ok(Some(Envelope { body: Response::Event { event }, .. })) => return Some((event, pipe)),
                    Ok(Some(_)) => continue,
                    _ => return None,
                }
            }
        }))
    }
}

type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// A connection shared by many tasks at once: each request only holds a lock while its frame is being
/// written, and responses are routed back by envelope id. A slow request (a big sort, an index upload)
/// therefore never blocks the others, and the service sees a newer search while an older one is still
/// running on the same connection (which is what lets it cancel the older one).
pub struct MuxClient {
    writer: tokio::sync::Mutex<WriteHalf<NamedPipeClient>>,
    waiters: Waiters,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    reader: tokio::task::JoinHandle<()>,
}

impl MuxClient {
    /// Takes over a connected client (typically right after its `Hello`).
    pub fn new(client: Client) -> MuxClient {
        let (mut rd, wr) = tokio::io::split(client.pipe);
        let waiters: Waiters = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let (w, c) = (waiters.clone(), closed.clone());
        let reader = tokio::spawn(async move {
            while let Ok(Some(env)) = read_frame::<_, Envelope<Response>>(&mut rd).await {
                if let Some(tx) = w.lock().unwrap().remove(&env.id) {
                    let _ = tx.send(env.body);
                }
            }
            // Mark closed before failing the waiters, so no request can slip in unanswered.
            c.store(true, Ordering::SeqCst);
            w.lock().unwrap().clear();
        });
        MuxClient {
            writer: tokio::sync::Mutex::new(wr),
            waiters,
            next_id: AtomicU64::new(client.next_id),
            closed,
            reader,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Sends one request and waits for its response; other requests may be in flight meanwhile.
    pub async fn request(&self, req: Request) -> io::Result<Response> {
        let gone = || io::Error::new(io::ErrorKind::UnexpectedEof, "service closed the connection");
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(id, tx);
        if self.is_closed() {
            self.waiters.lock().unwrap().remove(&id);
            return Err(gone());
        }
        let written = {
            let mut wr = self.writer.lock().await;
            write_frame(&mut *wr, &Envelope { id, body: req }).await
        };
        if let Err(e) = written {
            self.waiters.lock().unwrap().remove(&id);
            return Err(e);
        }
        rx.await.map_err(|_| gone())
    }
}

impl Drop for MuxClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
