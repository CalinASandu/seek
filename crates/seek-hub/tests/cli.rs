//! The `seek-hub` binary end to end: a real daemon on `127.0.0.1:0` in temp dirs, driven by its own CLI.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_seek-hub");

struct Dirs {
    _root: tempfile::TempDir,
    data: PathBuf,
    keys: PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let (data, keys) = (root.path().join("data"), root.path().join("keys"));
    Dirs { _root: root, data, keys }
}

fn hub(d: &Dirs, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args(args)
        .env("SEEK_HUB_DATA", &d.data)
        .env("SEEK_HUB_KEYS", &d.keys)
        .env("SEEK_HUB_LISTEN", "127.0.0.1:0")
        .env("RUST_LOG", "warn")
        .env_remove("SEEK_HUB_LAN")
        .env_remove("SEEK_HUB_PUBLIC")
        .env_remove("SEEK_HUB_NAME");
    c
}

fn run(d: &Dirs, args: &[&str]) -> Output {
    hub(d, args).stdin(Stdio::null()).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Identifies the admin endpoint: a Unix socket's inode (a socket cannot be read), the pipe-name file's bytes on Windows.
#[cfg(unix)]
fn endpoint_id(p: &Path) -> Vec<u8> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).unwrap().ino().to_le_bytes().to_vec()
}

#[cfg(windows)]
fn endpoint_id(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// A running daemon, killed when dropped.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `serve` and waits until its admin endpoint answers.
fn serve(d: &Dirs) -> Daemon {
    let child = hub(d, &["serve"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let daemon = Daemon(child);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if run(d, &["status", "--quiet"]).status.success() {
            return daemon;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the daemon did not answer within 20 s");
}

fn init_keys(d: &Dirs) {
    let out = run(d, &["init", "--keys"]);
    assert!(out.status.success(), "init --keys failed: {}", text(&out.stderr));
    assert!(d.keys.join("hub.key").exists());
}

fn status_json(d: &Dirs) -> serde_json::Value {
    let out = run(d, &["status", "--json"]);
    assert!(out.status.success(), "status failed: {}", text(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("status --json is not JSON ({e}): {}", text(&out.stdout)))
}

#[test]
fn serve_without_key_exits_2_with_hint() {
    let d = dirs();
    let out = run(&d, &["serve"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("No key in"), "{err}");
    assert!(err.contains("init --keys"), "{err}");
}

#[test]
fn init_keys_then_serve_then_status_json() {
    let d = dirs();
    init_keys(&d);
    // a second `init --keys` keeps the key
    let key = std::fs::read(d.keys.join("hub.key")).unwrap();
    assert!(run(&d, &["init", "--keys"]).status.success());
    assert_eq!(std::fs::read(d.keys.join("hub.key")).unwrap(), key);

    let _daemon = serve(&d);
    let s = status_json(&d);
    assert_eq!(s["pcCount"], 0, "{s}");
    assert_eq!(s["pcs"].as_array().map(Vec::len), Some(0), "{s}");
    for c in ["refusedHandshakes", "pairingFailures", "ownershipViolations", "quotaHits"] {
        assert!(s["counters"][c].is_u64(), "counter {c} missing: {s}");
    }
    assert_eq!(s["name"], "Seek Hub");
    assert!(run(&d, &["pcs"]).status.success());
}

#[test]
fn status_quiet_fails_without_daemon() {
    let d = dirs();
    init_keys(&d);
    let out = run(&d, &["status", "--quiet"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn link_prints_8_digit_code() {
    let d = dirs();
    init_keys(&d);
    let _daemon = serve(&d);
    let mut link = Daemon(hub(&d, &["link"]).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap());
    let out = BufReader::new(link.0.stdout.take().unwrap());
    let mut seen = Vec::new();
    let mut code = None;
    for line in out.lines() {
        let line = line.unwrap();
        if let Some(c) = find_code(&line) {
            code = Some(c);
            break;
        }
        seen.push(line);
    }
    let code = code.unwrap_or_else(|| panic!("no `1234 5678` code in: {seen:?}"));
    assert_eq!(code.len(), 9);
    let s = status_json(&d);
    assert_eq!(s["pairing"]["open"], true, "{s}");
}

/// The first `dddd dddd` in `line`.
fn find_code(line: &str) -> Option<String> {
    let b = line.as_bytes();
    (0..b.len().saturating_sub(8)).find_map(|i| {
        let w = &b[i..i + 9];
        let ok = w.iter().enumerate().all(|(j, c)| if j == 4 { *c == b' ' } else { c.is_ascii_digit() });
        let edge = |k: Option<&u8>| k.is_none_or(|c| !c.is_ascii_digit());
        (ok && edge(i.checked_sub(1).and_then(|k| b.get(k))) && edge(b.get(i + 9))).then(|| line[i..i + 9].to_string())
    })
}

#[test]
fn unlink_unknown_pc_fails() {
    let d = dirs();
    init_keys(&d);
    let _daemon = serve(&d);
    let out = run(&d, &["unlink", "Nobody's Laptop"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("Nobody's Laptop"), "{}", text(&out.stderr));
    assert!(!run(&d, &["unlink"]).status.success(), "unlink without a PC must fail");
}

#[test]
fn rotate_key_keeps_store_readable() {
    let d = dirs();
    init_keys(&d);
    let old_key = std::fs::read(d.keys.join("hub.key")).unwrap();
    let id = {
        let _daemon = serve(&d);
        let id = status_json(&d)["hubId"].clone();
        let out = run(&d, &["rotate-key"]);
        assert!(out.status.success(), "rotate-key failed: {}{}", text(&out.stdout), text(&out.stderr));
        id
    };
    assert_ne!(std::fs::read(d.keys.join("hub.key")).unwrap(), old_key, "the key did not change");
    assert!(!d.keys.join("hub.key.new").exists());
    assert_header_key(&d.data.join("hub.json"), &d.keys);

    // restart with the new key
    let _daemon = serve(&d);
    let s = status_json(&d);
    assert_eq!(s["hubId"], id, "a new hub was created instead of reading the old store");
    assert_eq!(s["pcCount"], 0);
}

#[test]
fn rotate_key_offline_then_doctor() {
    let d = dirs();
    init_keys(&d);
    drop(serve(&d));
    let out = run(&d, &["rotate-key"]);
    assert!(out.status.success(), "offline rotate-key failed: {}{}", text(&out.stdout), text(&out.stderr));
    assert_header_key(&d.data.join("host.key"), &d.keys);
    let _daemon = serve(&d);
    let out = run(&d, &["doctor"]);
    assert!(out.status.success(), "doctor failed: {}{}", text(&out.stdout), text(&out.stderr));
}

#[test]
fn second_serve_on_the_same_data_refused() {
    let d = dirs();
    init_keys(&d);
    let _daemon = serve(&d);
    let endpoint = std::fs::read_dir(&d.data)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("admin."))
        .unwrap();
    let before = endpoint_id(&d.data.join(&endpoint));
    let mut second = Daemon(hub(&d, &["serve"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap());
    let t = Instant::now();
    let status = loop {
        if let Some(s) = second.0.try_wait().unwrap() {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "a second serve on the same data directory kept running");
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut second.0.stderr.take().unwrap(), &mut err).unwrap();
    assert!(!status.success());
    assert!(err.contains("the hub is running"), "{err}");
    // the endpoint is still the first daemon's
    assert_eq!(endpoint_id(&d.data.join(&endpoint)), before);
    assert!(run(&d, &["status", "--quiet"]).status.success());
}

#[test]
fn locked_data_refuses_offline_rotate_and_init() {
    let d = dirs();
    init_keys(&d);
    drop(serve(&d));
    let key = std::fs::read(d.keys.join("hub.key")).unwrap();
    let files = |d: &Dirs| -> Vec<(PathBuf, Vec<u8>)> {
        let mut v: Vec<_> = std::fs::read_dir(&d.data)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json" || x == "key"))
            .map(|p| (p.clone(), std::fs::read(&p).unwrap()))
            .collect();
        v.sort();
        v
    };
    let data_before = files(&d);
    {
        // a daemon that holds the data directory but whose endpoint does not answer (starting, or lost)
        let _held = seek_hub::lock::DataLock::try_take(&d.data).unwrap();
        let out = run(&d, &["rotate-key"]);
        assert!(!out.status.success());
        assert!(text(&out.stderr).contains("the hub is running"), "{}", text(&out.stderr));
        let out = run(&d, &["init"]);
        assert!(!out.status.success());
        assert!(text(&out.stderr).contains("the hub is running"), "{}", text(&out.stderr));
        assert_eq!(std::fs::read(d.keys.join("hub.key")).unwrap(), key);
        assert!(!d.keys.join("hub.key.new").exists());
        assert_eq!(files(&d), data_before);
    }
    // released (as a killed process's lock is): works again
    assert!(run(&d, &["rotate-key"]).status.success());
}

#[test]
fn init_refused_during_unfinished_rotation() {
    let d = dirs();
    init_keys(&d);
    std::fs::write(d.keys.join("hub.key.new"), [7u8; 32]).unwrap();
    let out = run(&d, &["init"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("unfinished"), "{}", text(&out.stderr));
    assert!(!d.data.join("hub.json").exists(), "init wrote settings anyway");
}

#[test]
fn doctor_public_checks_the_handshake_reaches_this_hub() {
    let free_port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let d = dirs();
    init_keys(&d);
    // a plain public address (no TLS) pointing at the hub itself
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let start = |public: &str| {
        let child = hub(&d, &["serve"])
            .env("SEEK_HUB_LISTEN", &addr)
            .env("SEEK_HUB_PUBLIC", public)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Daemon(child);
        let t = Instant::now();
        while !run(&d, &["status", "--quiet"]).status.success() {
            assert!(t.elapsed() < Duration::from_secs(20), "the daemon did not answer");
            std::thread::sleep(Duration::from_millis(100));
        }
        daemon
    };
    {
        let _daemon = start(&addr);
        let out = run(&d, &["doctor", "--public"]);
        assert!(out.status.success(), "doctor --public failed: {}{}", text(&out.stdout), text(&out.stderr));
        assert!(text(&out.stdout).contains("reaches this hub"), "{}", text(&out.stdout));
    }

    // the public address answers, but it is not this hub
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_addr = other.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for c in other.incoming() {
            drop(c);
        }
    });
    let _daemon = start(&other_addr);
    let out = run(&d, &["doctor", "--public"]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains("did not reach this hub"), "{}", text(&out.stdout));
}

/// `path` is encrypted with the key in `keys/hub.key` (key id = first 8 bytes of BLAKE2s(key)).
fn assert_header_key(path: &Path, keys: &Path) {
    use blake2::{Blake2s256, Digest};
    let key = std::fs::read(keys.join("hub.key")).unwrap();
    let file = std::fs::read(path).unwrap();
    assert_eq!(&file[..8], b"SEEKENC1");
    assert_eq!(&file[8..16], &Blake2s256::digest(&key)[..8], "{} has another key", path.display());
}
