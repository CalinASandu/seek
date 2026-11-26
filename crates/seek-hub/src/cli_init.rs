//! `seek-hub init [--keys]` (part of the binary, not the library).

use std::io::{self, BufRead, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::ExitCode;

use seek_hub::config::{HubConfig, Settings, DEFAULT_PORT, SETTING_VARS};
use seek_hub::crypt::NEW_KEY_FILE;
use seek_hub::lock::{DataLock, LockError};
use seek_hub::{store, CryptError, HubKey};
use seek_sync::transport::HubAddr;

use crate::{fail, no_key_hint};

pub fn init_keys(cfg: &HubConfig) -> ExitCode {
    let keys = &cfg.keys_dir;
    match HubKey::create(keys) {
        Ok(_) => {
            println!("Created {}.", keys.join("hub.key").display());
            println!("Keep a copy somewhere safe: without it the stored data cannot be read (the fix is to wipe");
            println!("/data and link the PCs again). On the same SD card as /data it only stops casual reading.");
            ExitCode::SUCCESS
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            println!("{} already exists; it was kept.", keys.join("hub.key").display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(1, format!("Cannot create the key in {}: {e}. Is {} mounted writable?", keys.display(), keys.display())),
    }
}

pub fn init(base: &HubConfig) -> ExitCode {
    // hub.json is a store file: no daemon may write (or re-key) it meanwhile
    let _lock = match DataLock::try_take(&base.data_dir) {
        Ok(l) => l,
        Err(e @ LockError::Held(_)) => {
            return fail(
                1,
                format!(
                    "Refused: {e}. Stop it while you run init: docker compose stop seek-hub; docker compose run \
                     --rm --no-deps seek-hub init; docker compose start seek-hub"
                ),
            )
        }
        Err(e) => return fail(1, e),
    };
    let key = match HubKey::load(&base.keys_dir) {
        Ok(k) => k,
        Err(CryptError::NoKey) => return fail(2, no_key_hint(&base.keys_dir)),
        Err(e) => return fail(1, format!("Cannot read the key in {}: {e}", base.keys_dir.display())),
    };
    // files may already use the new key: let the hub finish the rotation before settings are written
    match HubKey::load_new(&base.keys_dir) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return fail(
                1,
                format!(
                    "A key rotation is unfinished ({} exists). Start the hub once (it finishes the rotation), \
                     or run seek-hub rotate-key, then run init again.",
                    base.keys_dir.join(NEW_KEY_FILE).display()
                ),
            )
        }
        Err(e) => return fail(1, format!("Cannot read {}: {e}", base.keys_dir.join(NEW_KEY_FILE).display())),
    }
    let saved = match store::load_settings(&base.data_dir, &key) {
        Ok(s) => s,
        Err(e) => return fail(1, format!("Cannot read hub.json: {e}. Run: seek-hub doctor")),
    };
    let now = match HubConfig::with_settings(|k| std::env::var(k).ok(), &saved) {
        Ok(c) => c,
        Err(e) => return fail(2, e),
    };
    println!("Seek Hub setup. Press Enter to keep the value in brackets.\n");
    let mut lines = io::stdin().lock().lines();
    let mut ask = |q: &str, current: &str| -> String {
        print!("{q} [{current}]: ");
        let _ = io::stdout().flush();
        match lines.next() {
            Some(Ok(l)) if !l.trim().is_empty() => l.trim().to_string(),
            Some(Ok(_)) => current.to_string(),
            // no terminal (EOF): keep everything
            _ => {
                println!();
                current.to_string()
            }
        }
    };
    let name = ask("Hub name", &now.name);
    let quota = loop {
        match ask("Storage per PC, MB", &(now.pc_quota_bytes / (1024 * 1024)).to_string()).parse::<u64>() {
            Ok(n) if n > 0 => break n,
            _ => println!("  a whole number, at least 1"),
        }
    };
    let devices = loop {
        match ask("Drives per PC", &now.pc_max_devices.to_string()).parse::<usize>() {
            Ok(n) if n > 0 => break n,
            _ => println!("  a whole number, at least 1"),
        }
    };
    let lan_now = if now.listen.iter().any(|a| a.ip().is_unspecified()) { "y" } else { "n" };
    let lan = ask(&format!("Also listen on the LAN, port {DEFAULT_PORT} (y/n)"), lan_now).to_ascii_lowercase().starts_with('y');
    let public = loop {
        let cur = now.public.as_ref().map_or("none".to_string(), |p| p.to_string());
        let a = ask("Public address, e.g. tls://seek-hub.tail1234.ts.net (none to clear)", &cur);
        if a.eq_ignore_ascii_case("none") || a == "-" {
            break None;
        }
        match HubAddr::parse(&a) {
            Ok(p) => break Some(p.to_string()),
            Err(e) => println!("  {e}"),
        }
    };
    let settings = Settings { name: Some(name), pc_quota_mb: Some(quota), pc_max_devices: Some(devices), lan: Some(lan), public };
    if let Err(e) = store::save_settings(&base.data_dir, &key, &settings) {
        return fail(1, format!("Cannot save hub.json: {e}"));
    }
    println!("\nSaved in {}.", base.data_dir.join("hub.json").display());
    for var in SETTING_VARS {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            println!("Note: {var} is set in the environment (compose.yaml) and wins over this setting.");
        }
    }
    println!("Restart the hub to apply: docker compose restart seek-hub\n");
    let cfg = HubConfig::with_settings(|k| std::env::var(k).ok(), &settings).unwrap_or(now);
    print!("{}", checklist());
    println!("\nAddresses for the PCs (Link a device -> Hub):");
    print_addrs(cfg.public.as_ref().map(|p| p.to_string()), &[], lan);
    ExitCode::SUCCESS
}

/// The Tailscale setup of spec §8, done once per tailnet.
fn checklist() -> String {
    "Tailscale checklist (once):\n\
     \x20 1. Admin console -> DNS: enable MagicDNS and HTTPS certificates.\n\
     \x20 2. Access controls: paste tailnet-policy.example.hujson. It defines tag:seek-hub, grants the funnel\n\
     \x20    attribute only to that tag and lets your own devices reach it on port 443 only.\n\
     \x20 3. Settings -> Keys: create a tagged (tag:seek-hub), single-use auth key; put it in .env as TS_AUTHKEY.\n\
     \x20    Tagged nodes do not expire.\n\
     \x20 4. funnel.json publishes only TLS-terminated TCP 443 -> 127.0.0.1:47321; docker compose up -d.\n\
     \x20 5. Check it from here: docker exec seek-hub seek-hub doctor --public\n"
        .to_string()
}

/// Prints where PCs can reach the hub: the public address, the specific listen addresses, and the host's
/// own addresses when it listens on every interface.
pub fn print_addrs(public: Option<String>, specific: &[String], lan: bool) {
    let mut any = false;
    if let Some(p) = &public {
        println!("  {p}   (public, through Tailscale Funnel)");
        any = true;
    }
    for a in specific.iter().filter(|a| Some(*a) != public.as_ref()) {
        println!("  {a}");
        any = true;
    }
    if lan {
        for ip in lan_ips() {
            println!("  {}   (LAN)", SocketAddr::new(ip, DEFAULT_PORT));
            any = true;
        }
        println!("  (in Docker that is the container's address; use the Pi's LAN IP with the published port)");
    }
    if !any {
        println!("  none: set SEEK_HUB_PUBLIC (Tailscale Funnel) or SEEK_HUB_LAN=1");
    }
}

/// This host's primary addresses: `connect` on a UDP socket sends nothing, it only picks the local address
/// that routes towards a (documentation-range) destination.
fn lan_ips() -> Vec<IpAddr> {
    [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")]
        .iter()
        .filter_map(|(bind, to)| {
            let s = UdpSocket::bind(bind).ok()?;
            s.connect(to).ok()?;
            s.local_addr().ok()
        })
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .collect()
}
