use crate::config::Config;
use crate::playlist::{Entry, Station};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::sync::{RwLock, watch};
use tracing::{info, warn};

const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(500);
const FAIL_COOLDOWN: Duration = Duration::from_secs(5);

pub struct StationRuntime {
    pub station: RwLock<Arc<Station>>,
    /// Last directory scan (all library files with durations), used by /admin.
    pub library: RwLock<Vec<(PathBuf, Duration)>>,
    pub presence: Presence,
    pub playing: AtomicBool,
    pub started: Instant,
    pub rescan_lock: tokio::sync::Mutex<()>,
    pub shutdown: watch::Sender<bool>,
    pub cfg: Config,
    run_counter: AtomicU64,
}

impl StationRuntime {
    pub fn new(station: Arc<Station>, cfg: Config, shutdown: watch::Sender<bool>) -> Self {
        StationRuntime {
            station: RwLock::new(station),
            library: RwLock::new(Vec::new()),
            presence: Presence::new(),
            playing: AtomicBool::new(false),
            started: Instant::now(),
            rescan_lock: tokio::sync::Mutex::new(()),
            shutdown,
            cfg,
            run_counter: AtomicU64::new(0),
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub async fn swap_station(&self, station: Arc<Station>) {
        *self.station.write().await = station;
    }

    fn next_run_id(&self) -> u64 {
        self.run_counter.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct Presence {
    sessions: Mutex<HashMap<String, Instant>>,
}

impl Presence {
    pub fn new() -> Self {
        Presence {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn heartbeat(&self, id: &str) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(id.to_string(), Instant::now());
        prune(&mut sessions);
        sessions.len()
    }

    pub fn leave(&self, id: &str) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(id);
        prune(&mut sessions);
        sessions.len()
    }

    pub fn count(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        prune(&mut sessions);
        sessions.len()
    }
}

fn prune(sessions: &mut HashMap<String, Instant>) {
    sessions.retain(|_, seen| seen.elapsed() < SESSION_TIMEOUT);
}

struct Active {
    child: Child,
    index: usize,
    base_offset: Duration,
    started: Instant,
}

pub async fn supervise(rt: Arc<StationRuntime>, mut shutdown: watch::Receiver<bool>) {
    let mut active: Option<Active> = None;
    let mut idle_since: Option<Instant> = None;
    let mut cooldown_until: Option<Instant> = None;

    loop {
        let viewers = rt.presence.count();
        let station = rt.station.read().await.clone();
        let position = station.position_at(SystemTime::now());

        if let Some(a) = active.as_mut() {
            match a.child.try_wait() {
                Ok(Some(status)) => {
                    let remaining = station.entries[a.index]
                        .duration
                        .saturating_sub(a.base_offset)
                        .saturating_sub(Duration::from_secs(10));
                    let early = a.started.elapsed() < remaining;
                    if !status.success() || early {
                        warn!(
                            "ffmpeg stopped early for '{}': {status}",
                            station.entries[a.index].title
                        );
                        cooldown_until = Some(Instant::now() + FAIL_COOLDOWN);
                    }
                    active = None;
                    rt.playing.store(false, Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("failed to poll ffmpeg: {err}");
                    active = None;
                    rt.playing.store(false, Ordering::Relaxed);
                }
            }
        }

        if viewers == 0 {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= rt.cfg.grace() && active.is_some() {
                info!("nobody is watching; ffmpeg goes back to sleep (Schrodinger mode)");
                kill(&mut active).await;
                rt.playing.store(false, Ordering::Relaxed);
                cooldown_until = None;
            }
        } else {
            idle_since = None;
            match position {
                None => {
                    kill(&mut active).await;
                    rt.playing.store(false, Ordering::Relaxed);
                }
                Some(pb) => {
                    let needs_restart = active
                        .as_ref()
                        .map(|a| a.index != pb.index)
                        .unwrap_or(true);
                    let cooling = cooldown_until.map(|t| Instant::now() < t).unwrap_or(false);
                    if needs_restart && !cooling {
                        kill(&mut active).await;
                        let entry = &station.entries[pb.index];
                        let run_id = rt.next_run_id();
                        match spawn_ffmpeg(&rt.cfg, entry, pb.offset, run_id).await {
                            Ok(child) => {
                                info!(
                                    "on air: '{}' at {:.0}s ({}/{}), {} viewer(s)",
                                    entry.title,
                                    pb.offset.as_secs_f64(),
                                    pb.index + 1,
                                    station.entries.len(),
                                    viewers
                                );
                                active = Some(Active {
                                    child,
                                    index: pb.index,
                                    base_offset: pb.offset,
                                    started: Instant::now(),
                                });
                                rt.playing.store(true, Ordering::Relaxed);
                            }
                            Err(err) => {
                                warn!("failed to start ffmpeg: {err:#}");
                                cooldown_until = Some(Instant::now() + FAIL_COOLDOWN);
                            }
                        }
                    }
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    kill(&mut active).await;
                    rt.playing.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

async fn spawn_ffmpeg(cfg: &Config, entry: &Entry, offset: Duration, run_id: u64) -> Result<Child> {
    clear_stream_dir(&cfg.temp_dir).await?;
    tokio::fs::create_dir_all(&cfg.temp_dir).await?;
    let playlist = cfg.temp_dir.join("index.m3u8");
    let segment = cfg.temp_dir.join(format!("seg_{run_id}_%05d.ts"));

    let mut cmd = Command::new("ffmpeg");
    #[cfg(target_os = "linux")]
    unsafe {
        // Safety: pre_exec runs the closure in the forked child before exec; the
        // only call here is async-signal-safe prctl().
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    cmd.arg("-hide_banner")
        .arg("-nostdin")
        .arg("-nostats")
        .arg("-loglevel")
        .arg("warning")
        .arg("-ss")
        .arg(format!("{:.3}", offset.as_secs_f64()))
        .arg("-re")
        .arg("-i")
        .arg(&entry.path)
        .args(["-map", "0:v:0?", "-map", "0:a:0?"])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
        ])
        .arg("-sc_threshold")
        .arg("0")
        .arg("-force_key_frames")
        .arg(format!("expr:gte(t,n_forced*{})", cfg.hls_time))
        .args(["-c:a", "aac", "-b:a", "160k", "-ac", "2"])
        .args(["-f", "hls"])
        .arg("-hls_time")
        .arg(cfg.hls_time.to_string())
        .args(["-hls_list_size", "6"])
        .args(["-hls_flags", "delete_segments+temp_file"])
        .arg("-hls_segment_filename")
        .arg(&segment)
        .arg(&playlist)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning ffmpeg for {}", entry.path.display()))?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "streamstation::ffmpeg", "{line}");
            }
        });
    }
    Ok(child)
}

async fn kill(active: &mut Option<Active>) {
    if let Some(mut a) = active.take() {
        let _ = a.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(3), a.child.wait()).await;
    }
}

pub async fn clear_stream_dir(dir: &Path) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".ts") || name.ends_with(".m3u8") || name.ends_with(".tmp") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}
