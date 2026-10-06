mod svc;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use seek_service::{default_data_dir, Config, RealVolumes};

#[derive(Parser)]
#[command(name = "seek-service", version, about = "Seek indexing service")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Run in the foreground with logs on the console (development).
    #[arg(long)]
    console: bool,
    /// Where devices.json and indexes live (default: %ProgramData%\Seek).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Pipe name to listen on.
    #[arg(long, default_value = seek_ipc::PIPE_NAME)]
    pipe: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register and start the Windows service (requires administrator).
    Install,
    /// Stop and remove the Windows service (requires administrator).
    Uninstall,
    /// Entry point used by the service control manager.
    Run,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Install) => svc::install(),
        Some(Cmd::Uninstall) => svc::uninstall(),
        Some(Cmd::Run) => svc::run_as_service(),
        None if cli.console => run_console(cli.data_dir.unwrap_or_else(default_data_dir), cli.pipe),
        None => {
            eprintln!("Use `seek-service --console` to run in the foreground, or `seek-service install`.");
            Ok(())
        }
    }
}

fn run_console(data_dir: PathBuf, pipe_name: String) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Ctrl+C: saving and stopping…");
            let _ = tx.send(true);
        });
        seek_service::run(Config { data_dir, pipe_name }, Arc::new(RealVolumes), rx).await
    })
}

pub(crate) fn init_file_logging(data_dir: &Path) {
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir.join("service.log")) {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .try_init();
    }
}
