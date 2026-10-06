//! Debounced recursive directory watcher (used to trigger re-walks of non-NTFS volumes/shares).

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_secs(2);

/// Calls `on_change` at most once per burst of activity, 2 s after the burst ends.
pub struct DirWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl DirWatcher {
    pub fn new(root: &Path, on_change: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let relevant = match &res {
                    Ok(ev) => !matches!(ev.kind, EventKind::Access(_)),
                    Err(_) => true, // e.g. buffer overflow: assume something changed
                };
                if relevant {
                    let _ = tx.send(());
                }
            })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        std::thread::Builder::new()
            .name("seek-dir-watch".into())
            .spawn(move || {
                // Ends when the watcher (and with it the sender) is dropped.
                while rx.recv().is_ok() {
                    loop {
                        match rx.recv_timeout(DEBOUNCE) {
                            Ok(()) => {}
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                on_change();
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn debounces_bursts() {
        let dir = tempfile::tempdir().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _w = DirWatcher::new(dir.path(), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

        for i in 0..20 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
        }
        std::thread::sleep(Duration::from_millis(3500));
        assert_eq!(count.load(Ordering::SeqCst), 1);

        std::fs::write(dir.path().join("late.txt"), b"y").unwrap();
        std::thread::sleep(Duration::from_millis(3500));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
