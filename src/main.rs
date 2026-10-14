mod broadcaster;
mod config;
mod playlist;
mod scan;
mod schedule;
mod web;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use playlist::{Station, deterministic_order};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "streamstation",
    version,
    about = "A local TV-style radio station for your video library"
)]
struct Cli {
    /// Path to config file (default: ~/.config/streamstation/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Verbose logging; also lists every unreachable file instead of just 5
    #[arg(long, global = true)]
    debug: bool,
    /// Serve only /admin (no playback, no empty-schedule shutdown) so an
    /// empty or broken schedule can be fixed while the server stays up
    #[arg(long, global = true)]
    admin_only: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the station and web server (default)
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        bind: Option<String>,
    },
    /// Scan the library, warm the duration cache, and print the schedule
    Scan,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                EnvFilter::new("streamstation=info,streamstation::ffmpeg=warn")
            }),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = Config::load(cli.config.as_deref())?;
    cfg.debug = cli.debug;
    cfg.admin_only = cli.admin_only;

    match cli.command {
        Some(Command::Serve { port, bind }) => {
            if let Some(port) = port {
                cfg.port = port;
            }
            if let Some(bind) = bind {
                cfg.bind = bind;
            }
            serve(cfg).await
        }
        Some(Command::Scan) => scan_cmd(cfg).await,
        None => serve(cfg).await,
    }
}

async fn ensure_tools(admin_only: bool) -> Result<()> {
    let mut tools = vec!["ffprobe"];
    if !admin_only {
        tools.push("ffmpeg");
    }
    for tool in tools {
        let ok = tokio::process::Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("{tool} not found on PATH; install ffmpeg first");
        }
    }
    Ok(())
}

async fn scan_cmd(cfg: Config) -> Result<()> {
    let items = scan::scan(&cfg.path, &cfg.extensions, &cfg.cache_file()).await?;
    let by_path: HashMap<PathBuf, Duration> = items.iter().cloned().collect();

    let entries = match schedule::ScheduleFile::load(&cfg.schedule_path)? {
        Some(sf) => sf.paths(),
        None => deterministic_order(items, cfg.shuffle_seed)
            .into_iter()
            .map(|(path, _)| path)
            .collect(),
    };

    let mut total = Duration::ZERO;
    let mut playable = 0usize;
    for (i, path) in entries.iter().enumerate() {
        match by_path.get(path) {
            Some(duration) => {
                playable += 1;
                total += *duration;
                println!(
                    "{:>4}. [{:>8}] {}  ({})",
                    i + 1,
                    fmt_duration(*duration),
                    title_of(path),
                    path.display()
                );
            }
            None => println!(
                "{:>4}. [ MISSING] {}  <-- unreachable, will be skipped",
                i + 1,
                path.display()
            ),
        }
    }
    println!(
        "\n{} titles in schedule ({} playable), {} of programming",
        entries.len(),
        playable,
        fmt_duration(total)
    );
    Ok(())
}

async fn serve(cfg: Config) -> Result<()> {
    ensure_tools(cfg.admin_only).await?;
    tokio::fs::create_dir_all(&cfg.temp_dir).await?;
    tokio::fs::create_dir_all(&cfg.cache_dir).await?;
    if !cfg.admin_only {
        broadcaster::clear_stream_dir(&cfg.temp_dir).await?;
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let placeholder = Station::new(Vec::new(), SystemTime::now());
    let rt = Arc::new(broadcaster::StationRuntime::new(
        Arc::new(placeholder),
        cfg.clone(),
        shutdown_tx,
    ));

    let supervisor = if cfg.admin_only {
        info!("admin-only mode: playback disabled, fix the schedule via /admin");
        None
    } else {
        Some(tokio::spawn(broadcaster::supervise(
            rt.clone(),
            shutdown_rx,
        )))
    };

    info!("loading schedule {} ...", cfg.schedule_path.display());
    let outcome = schedule::refresh(&rt).await?;
    if outcome.empty && !cfg.admin_only {
        schedule::fatal_empty_schedule(&rt, &outcome.unreachable).await;
    }
    info!(
        "station ready: {} playable, {} unreachable (skipped), {:.1} hours of programming",
        outcome.files,
        outcome.unreachable.len(),
        outcome.total_secs / 3600.0
    );

    let listener = tokio::net::TcpListener::bind(cfg.addr())
        .await
        .with_context(|| format!("binding {}", cfg.addr()))?;
    let local = listener.local_addr()?;
    let display_addr = if local.ip().is_unspecified() {
        format!("localhost:{}", local.port())
    } else {
        local.to_string()
    };
    info!("StreamStation is on air: http://{display_addr}/ (admin: /admin)");
    info!("nobody watching? ffmpeg is asleep - position is just a clock");

    axum::serve(listener, web::router(rt.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutting down");
    let _ = rt.shutdown.send(true);
    if let Some(supervisor) = supervisor {
        let _ = supervisor.await;
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn title_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn fmt_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
