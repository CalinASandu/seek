//! `seek-hub`: the hub daemon (`serve`, the default) and the admin commands that talk to it through the admin
//! endpoint (`docker exec seek-hub seek-hub <command>`; one-shot: `docker compose run --rm --no-deps seek-hub <command>`).
//! Exit codes: 0 ok, 1 failed, 2 usage / setup error.

mod cli_doctor;
mod cli_init;

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use seek_hub::admin::{self, AdminListener, AdminReq, AdminResp, ClientError, HubStatus, PcInfo};
use seek_hub::config::HubConfig;
use seek_hub::lock::{DataLock, LockError};
use seek_hub::net::Hub;
use seek_hub::pairing::PairOutcome;
use seek_hub::rotate::{self, KeyError, READ_ONLY_HINT};
use seek_hub::session::now_ms;
use seek_hub::{store, CryptError, Store};

const USAGE: &str = "\
usage: seek-hub [command]
  serve                  run the hub (the default)
  init --keys            create the key in SEEK_HUB_KEYS (once)
  init                   hub name, quotas, LAN, public address; prints the Tailscale checklist
  link                   open pairing for 2 min and print the code
  status [--json|--quiet]
  pcs [--json]           linked PCs
  unlink <name|host_id>  cut a PC off now
  rotate-key             re-encrypt everything with a new key
  doctor [--public]      check the key, the store, the listener, the disk (--public: the Funnel address)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env = |k: &str| std::env::var(k).ok();
    let cfg = match HubConfig::from_env(env) {
        Ok(c) => c,
        Err(e) => return fail(2, e),
    };
    match args.as_slice() {
        [] | ["serve"] => serve(cfg),
        ["init", "--keys"] => cli_init::init_keys(&cfg),
        ["init"] => cli_init::init(&cfg),
        ["link"] => client(link(&cfg)),
        ["status", flags @ ..] => match flags {
            [] => client(status(&cfg, false)),
            ["--json"] => client(status(&cfg, true)),
            ["--quiet"] => quiet(&cfg),
            _ => usage(),
        },
        ["pcs"] => client(pcs(&cfg, false)),
        ["pcs", "--json"] => client(pcs(&cfg, true)),
        ["unlink", pc] => client(unlink(&cfg, pc)),
        ["rotate-key"] => client(rotate_key(&cfg)),
        ["doctor"] => client(cli_doctor::doctor(&cfg, false)),
        ["doctor", "--public"] => client(cli_doctor::doctor(&cfg, true)),
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    fail(2, USAGE)
}

fn fail(code: u8, msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(code)
}

fn no_key_hint(keys: &Path) -> String {
    format!(
        "No key in {}. With the hub stopped, run: docker run --rm -v \"$PWD/data:/data\" -v \"$PWD/keys:/keys\" \
         ghcr.io/calinasandu/seek-hub:<version> init --keys",
        keys.display()
    )
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}

/// Runs an admin command; its `Err` is printed as is.
fn client(cmd: impl std::future::Future<Output = Result<(), String>>) -> ExitCode {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => return fail(1, e),
    };
    match rt.block_on(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(1, e),
    }
}

async fn ask(cfg: &HubConfig, req: AdminReq) -> Result<AdminResp, String> {
    match admin::request(&cfg.data_dir, req).await {
        Ok(AdminResp::Error { message }) => Err(message),
        Ok(r) => Ok(r),
        Err(e) => Err(e.to_string()),
    }
}

async fn hub_status(cfg: &HubConfig) -> Result<HubStatus, String> {
    match ask(cfg, AdminReq::Status).await? {
        AdminResp::Status(s) => Ok(s),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

// ---- serve ----

fn serve(base: HubConfig) -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(io::stdout).with_ansi(io::stdout().is_terminal()).init();

    // the only writer of this data directory from here until exit, before anything reads or rewrites it
    let lock = match DataLock::try_take(&base.data_dir) {
        Ok(l) => l,
        Err(e @ LockError::Held(_)) => return fail(1, format!("Not starting: {e}. One hub per data directory.")),
        Err(e) => return fail(1, e),
    };
    // finishes an interrupted key rotation first
    let key = match rotate::open_key(&base.keys_dir, &base.data_dir) {
        Ok(k) => k,
        Err(KeyError::Key(CryptError::NoKey)) => return fail(2, no_key_hint(&base.keys_dir)),
        Err(e) => return fail(1, format!("Cannot open the key in {}: {e}", base.keys_dir.display())),
    };
    let unreadable = |e: store::StoreError| {
        format!("The data in {} cannot be read: {e}. Run: seek-hub doctor", base.data_dir.display())
    };
    let settings = match store::load_settings(&base.data_dir, &key) {
        Ok(s) => s,
        Err(e) => return fail(1, unreadable(e)),
    };
    let cfg = match HubConfig::with_settings(|k| std::env::var(k).ok(), &settings) {
        Ok(c) => c,
        Err(e) => return fail(2, format!("{e} (in hub.json, saved by `seek-hub init`)")),
    };
    let store = match Store::open(&cfg, key) {
        Ok(s) => Arc::new(s),
        Err(e) => return fail(1, unreadable(e)),
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return fail(1, e),
    };
    match rt.block_on(run(cfg, store, &lock)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(1, e),
    }
}

async fn run(cfg: HubConfig, store: Arc<Store>, lock: &DataLock) -> Result<(), String> {
    let hub = Hub::start(cfg.clone(), store.clone()).await.map_err(|e| format!("Cannot listen: {e}"))?;
    let admin = admin::Admin::new(hub.clone(), store.clone(), cfg.clone());
    let listener = AdminListener::bind(&cfg.data_dir, lock)
        .await
        .map_err(|e| format!("Cannot open the admin endpoint in {}: {e}", cfg.data_dir.display()))?;
    tracing::info!("{} ({}) is running; {} PC(s) linked", store.name(), store.hub_id(), store.pcs().len());
    tokio::select! {
        _ = listener.run(admin) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted; stopping"),
        _ = terminated() => tracing::info!("SIGTERM; stopping"),
    }
    hub.shutdown();
    admin::remove_endpoint(&cfg.data_dir);
    Ok(())
}

/// SIGTERM (`docker stop`; as PID 1 the hub gets no default handling of it).
#[cfg(unix)]
async fn terminated() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn terminated() {
    std::future::pending().await
}

// ---- admin commands ----

async fn link(cfg: &HubConfig) -> Result<(), String> {
    let AdminResp::Link(info) = ask(cfg, AdminReq::Link).await? else {
        return Err("unexpected reply".into());
    };
    let c = &info.code;
    let secs = (info.expires_ms - now_ms()).max(0) / 1000;
    println!("Pairing is open. On the PC: Link a device -> Hub, enter the address, then this code:\n");
    println!("    {} {}\n", &c[..c.len().min(4)], &c[c.len().min(4)..]);
    println!("Valid for {secs} s; 3 wrong attempts close it. Addresses:");
    cli_init::print_addrs(info.public.clone(), &info.addrs, info.lan);
    println!("\nWaiting for the PC...");
    let give_up = info.expires_ms + 60_000;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let s = hub_status(cfg).await.map_err(|e| format!("Lost the hub while waiting: {e}"))?;
        if s.pairing.window != info.window {
            return Err("Another `seek-hub link` replaced this code.".into());
        }
        match s.pairing.outcome {
            Some(PairOutcome::Linked { name }) => {
                println!("Linked {name}.");
                return Ok(());
            }
            Some(PairOutcome::Expired) => return Err("The code expired; nothing was linked. Run `seek-hub link` again.".into()),
            Some(PairOutcome::TooManyFailures) => {
                return Err("3 wrong attempts closed pairing; nothing was linked. Run `seek-hub link` again.".into())
            }
            Some(PairOutcome::Failed { reason }) => return Err(format!("Pairing worked but linking failed: {reason}")),
            None if now_ms() > give_up => return Err("Pairing ended without an outcome.".into()),
            None => {}
        }
    }
}

/// `status --quiet`, the Docker healthcheck: exit 0 when the daemon answers, nothing printed.
fn quiet(cfg: &HubConfig) -> ExitCode {
    let ok = runtime().is_ok_and(|rt| rt.block_on(async { matches!(admin::request(&cfg.data_dir, AdminReq::Status).await, Ok(AdminResp::Status(_))) }));
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn status(cfg: &HubConfig, json: bool) -> Result<(), String> {
    let s = hub_status(cfg).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?);
        return Ok(());
    }
    println!("{} ({})", s.name, s.hub_id);
    println!("Listening:  {}", s.listening.join(", "));
    println!("Public:     {}", s.public.as_deref().unwrap_or("none (SEEK_HUB_PUBLIC)"));
    println!("PCs:        {} linked, {} drives, {} stored", s.pc_count, s.devices, size(s.stored_bytes));
    if let Some(free) = cli_doctor::free_bytes(&cfg.data_dir) {
        println!("Free disk:  {}", size(free));
    }
    let c = &s.counters;
    println!(
        "Counters:   {} refused handshakes, {} failed pairing attempts, {} ownership violations, {} quota hits",
        c.refused_handshakes, c.pairing_failures, c.ownership_violations, c.quota_hits
    );
    println!("Pairing:    {}", if s.pairing.open { "open" } else { "closed" });
    print_pcs(&s.pcs);
    Ok(())
}

async fn pcs(cfg: &HubConfig, json: bool) -> Result<(), String> {
    let AdminResp::Pcs { pcs } = ask(cfg, AdminReq::Pcs).await? else {
        return Err("unexpected reply".into());
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&pcs).map_err(|e| e.to_string())?);
    } else {
        print_pcs(&pcs);
    }
    Ok(())
}

fn print_pcs(pcs: &[PcInfo]) {
    if pcs.is_empty() {
        println!("No PCs linked yet (seek-hub link).");
    }
    for p in pcs {
        println!(
            "  {}  {} (user {}), last seen {}, {} drives, {}",
            p.host_id,
            p.name,
            p.user_name,
            ago(p.last_seen_ms),
            p.devices,
            size(p.stored_bytes)
        );
    }
}

async fn unlink(cfg: &HubConfig, pc: &str) -> Result<(), String> {
    match ask(cfg, AdminReq::Unlink { pc: pc.to_string() }).await? {
        AdminResp::Unlinked { name, host_id } => {
            println!("Unlinked {name} ({host_id}); its key is refused from now on.");
            Ok(())
        }
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Through the daemon when it runs; on the files directly when it does not (`docker compose run`).
async fn rotate_key(cfg: &HubConfig) -> Result<(), String> {
    let (files, resumed) = match admin::request(&cfg.data_dir, AdminReq::RotateKey).await {
        Ok(AdminResp::Rotated { files, resumed }) => (files, resumed),
        Ok(AdminResp::Error { message }) => return Err(message),
        Ok(other) => return Err(format!("unexpected reply: {other:?}")),
        Err(ClientError::NotRunning(_)) => {
            // "nothing answers" is not "nothing runs": only the data directory's lock proves no daemon writes
            let _lock = offline_lock(&cfg.data_dir)?;
            rotate_offline(cfg)?
        }
        Err(e) => return Err(e.to_string()),
    };
    let how = if resumed { "finished an interrupted rotation" } else { "new key" };
    println!("Key rotated ({how}): {files} files re-encrypted. Back up the new {}.", cfg.keys_dir.join("hub.key").display());
    Ok(())
}

/// The data directory's lock, for a command that writes the store without the daemon.
pub(crate) fn offline_lock(data: &Path) -> Result<DataLock, String> {
    DataLock::try_take(data).map_err(|e| match e {
        LockError::Held(_) => format!(
            "Refused: {e}, but its admin endpoint does not answer (still starting, or stuck). Stop it first: \
             docker compose stop seek-hub"
        ),
        other => other.to_string(),
    })
}

fn rotate_offline(cfg: &HubConfig) -> Result<(usize, bool), String> {
    match rotate::rotate_offline(&cfg.keys_dir, &cfg.data_dir) {
        Ok(r) => Ok(r),
        Err(KeyError::Key(CryptError::NoKey)) => Err(no_key_hint(&cfg.keys_dir)),
        Err(KeyError::Key(CryptError::Io(e))) => {
            Err(format!("Key rotation failed: {e}. Run rotate-key again to finish it.\n{READ_ONLY_HINT}"))
        }
        Err(e) => Err(format!("Key rotation failed: {e}. Run rotate-key again to finish it.")),
    }
}

// ---- helpers ----

fn size(b: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

fn ago(ms: i64) -> String {
    if ms <= 0 {
        return "never".into();
    }
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}
