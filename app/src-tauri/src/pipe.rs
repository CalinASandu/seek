//! Connection to seek-service: one multiplexed request connection (searches must stay on the
//! connection that created them; requests on it run concurrently) plus one event subscription that is
//! forwarded to the webview.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use seek_ipc::{Client, MuxClient, Request, Response, PROTOCOL_VERSION};
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::service_ctl;

pub const EVENT_CHANNEL: &str = "seek://event";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkState {
    Connected,
    Connecting,
    NotInstalled,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Connected => "connected",
            LinkState::Connecting => "connecting",
            LinkState::NotInstalled => "notInstalled",
        }
    }
}

pub struct ServiceLink {
    /// Locked only to (re)connect; requests run on a clone of the `Arc` without holding it.
    client: tokio::sync::Mutex<Option<Arc<MuxClient>>>,
    state: Mutex<LinkState>,
}

impl Default for ServiceLink {
    fn default() -> Self {
        Self { client: tokio::sync::Mutex::new(None), state: Mutex::new(LinkState::Connecting) }
    }
}

async fn hello(mut c: Client) -> std::io::Result<Client> {
    match c.request(Request::Hello { protocol_version: PROTOCOL_VERSION }).await? {
        Response::Hello { .. } => Ok(c),
        Response::Error { message } => Err(std::io::Error::other(message)),
        other => Err(std::io::Error::other(format!("unexpected hello reply: {other:?}"))),
    }
}

impl ServiceLink {
    pub fn state(&self) -> LinkState {
        *self.state.lock().unwrap()
    }

    fn set_state(&self, app: &AppHandle, s: LinkState) {
        let changed = {
            let mut cur = self.state.lock().unwrap();
            std::mem::replace(&mut *cur, s) != s
        };
        if changed {
            let _ = app.emit(EVENT_CHANNEL, json!({ "type": "serviceState", "state": s.as_str() }));
        }
    }

    async fn connection(&self) -> Result<Arc<MuxClient>, String> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = Client::connect().await.map_err(|e| format!("Seek service unavailable: {e}"))?;
        let c = Arc::new(MuxClient::new(hello(c).await.map_err(|e| e.to_string())?));
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Sends one request, connecting on demand. Requests run concurrently on the shared connection;
    /// a broken connection is dropped so the next call reconnects.
    pub async fn request(&self, req: Request) -> Result<Response, String> {
        let conn = self.connection().await?;
        match conn.request(req).await {
            Ok(Response::Error { message }) => Err(message),
            Ok(r) => Ok(r),
            Err(e) => {
                let mut guard = self.client.lock().await;
                if guard.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                    *guard = None;
                }
                Err(format!("lost connection to Seek service: {e}"))
            }
        }
    }

    /// Keeps an event subscription alive and mirrors connection state to the UI.
    pub async fn run_events(&self, app: AppHandle) {
        let mut backoff = Duration::from_millis(250);
        loop {
            if let Ok(c) = Client::connect().await {
                match hello(c).await {
                    Ok(c) => match c.subscribe().await {
                        Ok(stream) => {
                            backoff = Duration::from_millis(250);
                            self.set_state(&app, LinkState::Connected);
                            // Anything cached before the (re)connect is stale.
                            let _ = app.emit(EVENT_CHANNEL, json!({ "type": "devicesChanged" }));
                            let _ = app.emit(EVENT_CHANNEL, json!({ "type": "resultsInvalidated" }));
                            let mut stream = std::pin::pin!(stream);
                            while let Some(event) = stream.next().await {
                                let _ = app.emit(EVENT_CHANNEL, &event);
                            }
                            *self.client.lock().await = None;
                        }
                        Err(e) => tracing::warn!("subscribe failed: {e}"),
                    },
                    Err(e) => tracing::warn!("hello failed: {e}"),
                }
            }
            let s = if service_ctl::is_installed() { LinkState::Connecting } else { LinkState::NotInstalled };
            // In dev the service runs in a console without being installed: keep "connecting"
            // once we have been connected, so the UI shows a spinner rather than the setup card.
            let s = if s == LinkState::NotInstalled && self.state() == LinkState::Connected {
                LinkState::Connecting
            } else {
                s
            };
            self.set_state(&app, s);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}
