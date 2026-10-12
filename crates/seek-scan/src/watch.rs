//! Debounced recursive directory watcher (used to keep walked volumes and shares current).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_secs(2);

/// More changed folders than this in one burst: rescanning them one by one is no longer cheaper.
pub const MAX_CHANGED_DIRS: usize = 2000;

/// What changed during one burst of activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    /// The folders whose direct entries changed (an entry was added, removed, renamed or modified).
    Dirs(HashSet<PathBuf>),
    /// Too much changed, or the OS dropped events (buffer overflow): only a full rescan is safe.
    Everything,
}

/// Calls `on_change` at most once per burst of activity, 2 s after the burst ends.
pub struct DirWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl DirWatcher {
    pub fn new(root: &Path, on_change: impl Fn(Changed) + Send + 'static) -> notify::Result<Self> {
        // `None` = events were lost.
        let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let msg = match res {
                    Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => return,
                    Ok(ev) => Some(ev.paths),
                    Err(_) => None,
                };
                let _ = tx.send(msg);
            })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        let root = root.to_path_buf();
        std::thread::Builder::new()
            .name("seek-dir-watch".into())
            .spawn(move || {
                // Ends when the watcher (and with it the sender) is dropped.
                while let Ok(first) = rx.recv() {
                    let mut burst = Burst::default();
                    burst.add(&root, first);
                    loop {
                        match rx.recv_timeout(DEBOUNCE) {
                            Ok(msg) => burst.add(&root, msg),
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                on_change(burst.finish());
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

#[derive(Default)]
struct Burst {
    dirs: HashSet<PathBuf>,
    everything: bool,
}

impl Burst {
    fn add(&mut self, root: &Path, msg: Option<Vec<PathBuf>>) {
        let Some(paths) = msg else {
            self.everything = true;
            return;
        };
        if self.everything {
            return;
        }
        for p in paths {
            // The folder that lists `p` changed; the root itself is listed by nothing.
            let dir = match p.parent() {
                Some(parent) if p != root && parent.starts_with(root) => parent.to_path_buf(),
                _ => root.to_path_buf(),
            };
            self.dirs.insert(dir);
        }
        if self.dirs.len() > MAX_CHANGED_DIRS {
            self.everything = true;
            self.dirs.clear();
        }
    }

    fn finish(self) -> Changed {
        if self.everything {
            Changed::Everything
        } else {
            Changed::Dirs(self.dirs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn debounces_bursts_and_reports_changed_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let seen: Arc<Mutex<Vec<Changed>>> = Arc::default();
        let s = seen.clone();
        let _w = DirWatcher::new(&root, move |c| s.lock().unwrap().push(c)).unwrap();

        for i in 0..20 {
            std::fs::write(root.join(format!("f{i}.txt")), b"x").unwrap();
        }
        std::fs::write(root.join("sub").join("deep.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(3500));
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            let Changed::Dirs(dirs) = &seen[0] else { panic!("{:?}", seen[0]) };
            assert!(dirs.contains(&root), "{dirs:?}");
            assert!(dirs.contains(&root.join("sub")), "{dirs:?}");
        }

        std::fs::write(root.join("late.txt"), b"y").unwrap();
        std::thread::sleep(Duration::from_millis(3500));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn lost_events_or_too_many_folders_mean_everything() {
        let root = PathBuf::from(r"C:\r");
        let mut b = Burst::default();
        b.add(&root, Some(vec![root.join("a").join("x.txt")]));
        b.add(&root, None);
        b.add(&root, Some(vec![root.join("b.txt")]));
        assert_eq!(b.finish(), Changed::Everything);

        let mut b = Burst::default();
        for i in 0..=MAX_CHANGED_DIRS {
            b.add(&root, Some(vec![root.join(i.to_string()).join("f")]));
        }
        assert_eq!(b.finish(), Changed::Everything);

        let mut b = Burst::default();
        b.add(&root, Some(vec![root.clone(), root.join("top.txt")]));
        assert_eq!(b.finish(), Changed::Dirs([root].into_iter().collect()));
    }
}
