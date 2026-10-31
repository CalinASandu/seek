//! `seek-hub doctor [--public]` (part of the binary, not the library).

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use seek_hub::admin::HubStatus;
use seek_hub::config::HubConfig;
use seek_hub::crypt::NEW_KEY_FILE;
use seek_hub::rotate;
use seek_hub::{CryptError, HubKey};
use seek_sync::transport::{self, HubAddr, TlsConfig};

use crate::{hub_status, no_key_hint, size};

/// `doctor` wants at least this much free space on the data volume.
const MIN_FREE: u64 = 1024 * 1024 * 1024;

pub async fn doctor(cfg: &HubConfig, public: bool) -> Result<(), String> {
    let mut failed = 0;
    let mut check = |ok: bool, what: String| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        failed += usize::from(!ok);
    };
    let key = HubKey::load(&cfg.keys_dir);
    let new = HubKey::load_new(&cfg.keys_dir).ok().flatten();
    match &key {
        Ok(_) => check(true, format!("key loads ({})", cfg.keys_dir.join("hub.key").display())),
        Err(CryptError::NoKey) => check(false, no_key_hint(&cfg.keys_dir)),
        Err(e) => check(false, format!("key: {e}")),
    }
    if new.is_some() {
        println!("     an unfinished key rotation ({NEW_KEY_FILE}): finished by rotate-key or the next start");
    }
    if let Ok(key) = &key {
        let keys: Vec<&HubKey> = std::iter::once(key).chain(new.as_ref()).collect();
        match rotate::verify_dir(&cfg.data_dir, &keys) {
            Ok((n, bad)) if bad.is_empty() => check(true, format!("store: all {n} files decrypt")),
            Ok((n, bad)) => check(
                false,
                format!(
                    "store: {} of {n} files do not decrypt:\n       {}\n       Data without its key is unreadable: \
                     wipe /data and link the PCs again (they re-upload).",
                    bad.len(),
                    bad.join("\n       ")
                ),
            ),
            Err(e) => check(false, format!("store: {e}")),
        }
    }
    let status = hub_status(cfg).await;
    match &status {
        Ok(_) => check(true, "admin endpoint answers".into()),
        Err(e) => check(false, format!("admin endpoint: {e}")),
    }
    // before the listener check below, whose connection the hub also counts as refused
    if public {
        let addr = status.as_ref().ok().and_then(|s| s.public.clone()).or_else(|| cfg.public.as_ref().map(|p| p.to_string()));
        match (addr, &status) {
            (None, _) => check(false, "public address: SEEK_HUB_PUBLIC is not set".into()),
            (Some(_), Err(_)) => check(false, "public address: the hub is not running".into()),
            (Some(a), Ok(s)) => {
                let (ok, what) = check_public(cfg, &a, s).await;
                check(ok, what);
            }
        }
    }
    for a in status.as_ref().map_or(&[][..], |s| &s.listening[..]) {
        let Ok(mut addr) = a.parse::<SocketAddr>() else { continue };
        if addr.ip().is_unspecified() {
            addr.set_ip([127, 0, 0, 1].into());
        }
        let up = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok();
        let how = if up { "accepts connections (counted as a refused handshake)" } else { "does not answer" };
        check(up, format!("listener {a} {how}"));
    }
    match free_bytes(&cfg.data_dir) {
        Some(free) => check(free > MIN_FREE, format!("free disk on {}: {}", cfg.data_dir.display(), size(free))),
        None => println!("--   free disk: not measured on this system"),
    }
    if failed > 0 {
        return Err(format!("{failed} check(s) failed"));
    }
    Ok(())
}

/// `doctor --public`, the only code that dials out (the daemon never does): TLS to the public address (TLS 1.3,
/// certificate and hostname checked), then a Noise handshake with a throwaway key. The hub drops that key in
/// silence, as it does every stranger's, so the proof that it reached *this* hub is its refused-handshake
/// counter going up.
async fn check_public(cfg: &HubConfig, addr: &str, s: &HubStatus) -> (bool, String) {
    use tokio::io::AsyncWriteExt;
    let parsed = match HubAddr::parse(addr) {
        Ok(a) => a,
        Err(e) => return (false, format!("public address {addr}: {e}")),
    };
    let Some(hub_key) = seek_sync::peers::key_from_b64(&s.public_key) else {
        return (false, "the hub reported a bad public key".into());
    };
    let mut io = match transport::dial(&parsed, &TlsConfig::public(), Duration::from_secs(15)).await {
        Ok(io) => io,
        Err(seek_sync::Error::Untrusted(e)) => return (false, format!("{addr}: certificate refused: {e}")),
        Err(e) => return (false, format!("{addr}: cannot connect: {e}")),
    };
    if matches!(parsed, HubAddr::Tls { .. }) {
        println!("ok   {addr}: TLS 1.3, certificate valid for its name");
    }
    // fresh: earlier connections may have been counted since `s`
    let before = hub_status(cfg).await.map_or(s.counters.refused_handshakes, |s| s.counters.refused_handshakes);
    let me = seek_sync::Keys::generate();
    let _ = io.write_all(&[seek_hub::net::SESSION]).await;
    // the hub never answers this key: the handshake ends with the hub closing the connection
    let _ = tokio::time::timeout(Duration::from_secs(10), seek_sync::noise::connect(io, &me, &hub_key)).await;
    for _ in 0..20 {
        if hub_status(cfg).await.is_ok_and(|s| s.counters.refused_handshakes > before) {
            return (true, format!("{addr} reaches this hub (it refused the test key, as it should)"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    (false, format!("{addr}: a handshake sent there did not reach this hub (another server, or Funnel is off)"))
}


/// Free space for an unprivileged user on the volume holding `dir`.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // the statvfs field types differ between targets
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is NUL-terminated and `st` is a plain C struct the call fills in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}
