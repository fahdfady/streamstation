mod broadcaster;
mod config;
mod playlist;
mod scan;
mod web;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use playlist::{Station, deterministic_order};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tracing::{info, warn};
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
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("streamstation=info,streamstation::ffmpeg=warn")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = Config::load(cli.config.as_deref())?;

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

async fn ensure_ffmpeg() -> Result<()> {
    for tool in ["ffmpeg", "ffprobe"] {
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
    let cache_path = cfg.cache_dir.join("movies.json");
    let items = scan::scan(&cfg.path, &cfg.extensions, &cache_path).await?;
    let ordered = deterministic_order(items, cfg.shuffle_seed);
    let station = Station::new(ordered, SystemTime::now());

    for (i, entry) in station.entries.iter().enumerate() {
        println!(
            "{:>4}. [{:>8}] {}  ({})",
            i + 1,
            fmt_duration(entry.duration),
            entry.title,
            entry.path.display()
        );
    }
    println!(
        "\n{} titles, {} of programming",
        station.entries.len(),
        fmt_duration(station.total)
    );
    Ok(())
}

async fn serve(cfg: Config) -> Result<()> {
    ensure_ffmpeg().await?;
    tokio::fs::create_dir_all(&cfg.temp_dir).await?;
    tokio::fs::create_dir_all(&cfg.cache_dir).await?;
    broadcaster::clear_stream_dir(&cfg.temp_dir).await?;

    let cache_path = cfg.cache_dir.join("movies.json");
    info!("scanning {} ...", cfg.path.display());
    let items = scan::scan(&cfg.path, &cfg.extensions, &cache_path).await?;
    if items.is_empty() {
        warn!(
            "no playable videos found in {} - check the path and file extensions",
            cfg.path.display()
        );
    }

    let ordered = deterministic_order(items, cfg.shuffle_seed);
    let station = Station::new(ordered, SystemTime::now());
    info!(
        "station built: {} titles, {:.1} hours of programming",
        station.entries.len(),
        station.total.as_secs_f64() / 3600.0
    );

    let rt = Arc::new(broadcaster::StationRuntime::new(Arc::new(station), cfg.clone()));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let supervisor = tokio::spawn(broadcaster::supervise(rt.clone(), shutdown_rx));

    let listener = tokio::net::TcpListener::bind(cfg.addr())
        .await
        .with_context(|| format!("binding {}", cfg.addr()))?;
    let local = listener.local_addr()?;
    let display_addr = if local.ip().is_unspecified() {
        format!("localhost:{}", local.port())
    } else {
        local.to_string()
    };
    info!("StreamStation is on air: http://{display_addr}/");
    info!("nobody watching? ffmpeg is asleep - position is just a clock");

    axum::serve(listener, web::router(rt.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutting down");
    let _ = shutdown_tx.send(true);
    let _ = supervisor.await;
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

fn fmt_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
