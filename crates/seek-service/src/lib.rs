//! Seek background service: owns every device index, keeps them current and answers searches
//! over a named pipe.

pub mod access;
mod journal_ops;
pub mod monitor;
pub mod peers;
pub mod server;
pub mod state;
pub mod sync_host;
#[cfg(test)]
mod test_support;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub use monitor::{RealVolumes, VolumeSource};
pub use state::{Config, State, SyncConfig};

const VOLUME_POLL: Duration = Duration::from_secs(2);
const JOURNAL_POLL: Duration = Duration::from_millis(500);
const SAVE_EVERY: Duration = Duration::from_secs(5 * 60);

/// TCP port linked PCs connect to (the firewall rule opens it on private networks).
pub const SYNC_PORT: u16 = 47321;

/// Sync settings of the real service: all interfaces, the standard port, mDNS, this computer's name.
pub fn default_sync_config() -> SyncConfig {
    SyncConfig {
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        port: SYNC_PORT,
        mdns: true,
        name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
        interval: Duration::from_secs(60),
    }
}

/// Default data directory: `%ProgramData%\Seek`.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Seek")
}

/// Runs the service until `shutdown` flips to `true`, then saves everything.
pub async fn run(
    cfg: Config,
    source: Arc<dyn VolumeSource>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    run_with(cfg, source, shutdown, None).await
}

/// [`run`], handing the loaded state to `ready` once the service (and sync, if enabled) is up — tests reach
/// the sync runtime through `State::peers_rt`.
pub async fn run_with(
    cfg: Config,
    source: Arc<dyn VolumeSource>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ready: Option<tokio::sync::oneshot::Sender<Arc<State>>>,
) -> anyhow::Result<()> {
    let state = tokio::task::spawn_blocking(move || State::load(cfg, source)).await??;
    tracing::info!(
        "data dir {}, {} known devices",
        state.cfg.data_dir.display(),
        state.registry.read().unwrap().devices.len()
    );

    let mut background = Vec::new();
    background.push(every(VOLUME_POLL, state.clone(), monitor::poll_volumes));
    background.push(every(JOURNAL_POLL, state.clone(), monitor::poll_journals));
    background.push(every(SAVE_EVERY, state.clone(), |s| s.save_dirty()));
    background.push(every(Duration::from_secs(1), state.clone(), |s| {
        if s.invalidated.swap(false, Ordering::Relaxed) {
            s.emit(seek_ipc::Event::ResultsInvalidated);
        }
    }));

    let peers_rt = match state.cfg.sync.clone() {
        Some(sync) => match peers::PeersRuntime::start(state.clone(), sync).await {
            Ok(rt) => Some(rt),
            Err(e) => {
                tracing::error!("sync with linked PCs unavailable: {e}");
                None
            }
        },
        None => None,
    };
    if let Some(tx) = ready {
        let _ = tx.send(state.clone());
    }

    let served = server::serve(state.clone(), shutdown).await;

    if let Some(rt) = peers_rt {
        rt.shutdown();
    }

    for task in background {
        task.abort();
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || st.save_dirty()).await?;
    tracing::info!("stopped");
    served
}

fn every(period: Duration, state: Arc<State>, f: fn(&Arc<State>)) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let st = state.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || f(&st)).await {
                tracing::error!("background task panicked: {e}");
            }
        }
    })
}
