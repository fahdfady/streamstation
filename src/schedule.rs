use crate::broadcaster::StationRuntime;
use crate::playlist::{Station, deterministic_order};
use crate::scan;
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ScheduleFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_unix: Option<u64>,
    /// Present while the station is paused; the instant the playhead froze.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at_unix: Option<u64>,
    #[serde(default)]
    pub entries: Vec<EntryLine>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EntryLine {
    pub path: PathBuf,
}

impl ScheduleFile {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading schedule {}", path.display()))?;
        let parsed = toml::from_str(&text)
            .with_context(|| format!("parsing schedule {}", path.display()))?;
        Ok(Some(parsed))
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|e| e.path.clone()).collect()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing schedule")?;
        std::fs::write(path, text).with_context(|| format!("writing schedule {}", path.display()))
    }
}

#[derive(Serialize, Clone)]
pub struct ApplyOutcome {
    pub files: usize,
    pub total_secs: f64,
    pub unreachable: Vec<PathBuf>,
    pub empty: bool,
}

#[derive(Serialize)]
pub struct ScheduleEntryView {
    pub index: usize,
    pub path: PathBuf,
    pub title: String,
    pub duration_secs: Option<f64>,
    pub reachable: bool,
    /// Index in the station (playable entries only); None when unreachable.
    pub station_index: Option<usize>,
}

#[derive(Serialize)]
pub struct ScheduleView {
    pub entries: Vec<ScheduleEntryView>,
    pub total_secs: f64,
    pub epoch_unix: Option<u64>,
}

pub async fn view(rt: &StationRuntime) -> Result<ScheduleView> {
    let schedule = ScheduleFile::load(&rt.cfg.schedule_path)?.unwrap_or_default();
    let station = rt.station.read().await.clone();
    let playable: HashMap<PathBuf, (usize, Duration)> = station
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.clone(), (i, e.duration)))
        .collect();

    let entries = schedule
        .paths()
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let playable = playable.get(&path).copied();
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ScheduleEntryView {
                index,
                path,
                title,
                duration_secs: playable.map(|(_, d)| d.as_secs_f64()),
                reachable: playable.is_some(),
                station_index: playable.map(|(i, _)| i),
            }
        })
        .collect();

    Ok(ScheduleView {
        entries,
        total_secs: station.total.as_secs_f64(),
        epoch_unix: schedule.epoch_unix,
    })
}

pub async fn apply_entries(rt: &Arc<StationRuntime>, entries: Vec<PathBuf>) -> Result<ApplyOutcome> {
    let _guard = rt.rescan_lock.lock().await;
    apply_locked(rt, entries).await
}

/// Rescan the media directory, store the library listing, reload schedule.toml
/// (picking up hand edits) and apply it.
pub async fn refresh(rt: &Arc<StationRuntime>) -> Result<ApplyOutcome> {
    let _guard = rt.rescan_lock.lock().await;
    let cfg = &rt.cfg;
    let items = scan::scan(&cfg.path, &cfg.extensions, &cfg.cache_file()).await?;
    *rt.library.write().await = items.clone();

    let entries = match ScheduleFile::load(&cfg.schedule_path)? {
        Some(schedule) => schedule.paths(),
        None => deterministic_order(items, cfg.shuffle_seed)
            .into_iter()
            .map(|(path, _)| path)
            .collect(),
    };
    apply_locked(rt, entries).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShuffleMode {
    /// Back to the deterministic order derived from the configured seed.
    Reset,
    /// A fresh, unseeded random order.
    Random,
}

pub async fn shuffle(rt: &Arc<StationRuntime>, mode: ShuffleMode) -> Result<ApplyOutcome> {
    let _guard = rt.rescan_lock.lock().await;
    let cfg = &rt.cfg;
    let schedule = ScheduleFile::load(&cfg.schedule_path)?.unwrap_or_default();
    let paths = schedule.paths();

    let entries = match mode {
        ShuffleMode::Reset => deterministic_order(
            paths.into_iter().map(|path| (path, ())).collect(),
            cfg.shuffle_seed,
        )
        .into_iter()
        .map(|(path, _)| path)
        .collect(),
        ShuffleMode::Random => {
            let mut paths = paths;
            paths.shuffle(&mut rand::rng());
            paths
        }
    };
    apply_locked(rt, entries).await
}

#[derive(Serialize)]
pub struct SeekOutcome {
    pub index: usize,
    pub offset_secs: f64,
    pub title: String,
}

/// Move the station's playhead like a VLC seek: re-anchor the epoch so the
/// wall-clock model itself now reads `index` + `offset`. Persisted in
/// schedule.toml, so the new position survives restarts. The station swap
/// bumps the generation, which makes the supervisor restart ffmpeg and every
/// viewer re-attach - even when the movie index did not change.
pub async fn seek(rt: &Arc<StationRuntime>, index: usize, offset_secs: f64) -> Result<SeekOutcome> {
    let _guard = rt.rescan_lock.lock().await;
    let cfg = &rt.cfg;

    let station = rt.station.read().await.clone();
    let entry = station
        .entries
        .get(index)
        .with_context(|| format!("no movie at station index {index}"))?;
    let offset = Duration::from_secs_f64(offset_secs.clamp(0.0, entry.duration.as_secs_f64()));
    let target = entry.start + offset;
    let title = entry.title.clone();

    // While paused the playhead is frozen, so anchor the seek to the pause instant.
    let anchor = station.paused_at.unwrap_or_else(SystemTime::now);
    let new_epoch = reanchored_epoch(target, anchor);
    let mut schedule = ScheduleFile::load(&cfg.schedule_path)?.unwrap_or_default();
    schedule.epoch_unix = Some(time_to_unix(new_epoch));
    schedule.paused_at_unix = station.paused_at.map(time_to_unix);
    schedule.save(&cfg.schedule_path)?;

    let entries: Vec<(PathBuf, Duration)> = station
        .entries
        .iter()
        .map(|e| (e.path.clone(), e.duration))
        .collect();
    rt.swap_station(Arc::new(Station::new_with(entries, new_epoch, station.paused_at)))
        .await;

    info!(
        "seek: '{}' at {:.0}s (grid re-anchored)",
        title,
        offset.as_secs_f64()
    );
    Ok(SeekOutcome {
        index,
        offset_secs: offset.as_secs_f64(),
        title,
    })
}

fn reanchored_epoch(target: Duration, now: SystemTime) -> SystemTime {
    now.checked_sub(target).unwrap_or(UNIX_EPOCH)
}

/// Where the playhead sat when the station was paused.
fn frozen_elapsed(epoch: SystemTime, paused_at: SystemTime, total: Duration) -> Duration {
    if total.is_zero() {
        return Duration::ZERO;
    }
    let elapsed = paused_at.duration_since(epoch).unwrap_or(Duration::ZERO);
    Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64)
}

/// Resuming re-anchors the epoch so playback continues from the frozen position
/// instead of jumping forward by however long the pause lasted.
fn resume_epoch(
    epoch: SystemTime,
    paused_at: SystemTime,
    now: SystemTime,
    total: Duration,
) -> SystemTime {
    now.checked_sub(frozen_elapsed(epoch, paused_at, total))
        .unwrap_or(UNIX_EPOCH)
}

#[derive(Serialize)]
pub struct PauseOutcome {
    pub paused: bool,
    pub index: Option<usize>,
    pub offset_secs: f64,
    pub title: Option<String>,
}

/// Pause or resume the whole station. Paused freezes the playhead (persisted in
/// schedule.toml, so it survives restarts); resuming continues from where it froze.
pub async fn set_paused(rt: &Arc<StationRuntime>, paused: bool) -> Result<PauseOutcome> {
    let _guard = rt.rescan_lock.lock().await;
    let cfg = &rt.cfg;
    let mut schedule = ScheduleFile::load(&cfg.schedule_path)?.unwrap_or_default();
    let now = SystemTime::now();

    if paused {
        if schedule.paused_at_unix.is_none() {
            schedule.paused_at_unix = Some(time_to_unix(now));
            info!("station paused");
        }
    } else if let Some(paused_at) = schedule.paused_at_unix.map(unix_to_time) {
        let epoch = schedule.epoch_unix.map(unix_to_time).unwrap_or(paused_at);
        let total = rt.station.read().await.total;
        schedule.epoch_unix = Some(time_to_unix(resume_epoch(epoch, paused_at, now, total)));
        schedule.paused_at_unix = None;
        info!("station resumed");
    }
    schedule.save(&cfg.schedule_path)?;
    apply_locked(rt, schedule.paths()).await?;

    let station = rt.station.read().await.clone();
    let (index, offset_secs, title) = match station.position_at(now) {
        Some(pb) => {
            let entry = &station.entries[pb.index];
            (Some(pb.index), pb.offset.as_secs_f64(), Some(entry.title.clone()))
        }
        None => (None, 0.0, None),
    };
    Ok(PauseOutcome {
        paused: station.is_paused(),
        index,
        offset_secs,
        title,
    })
}

async fn apply_locked(rt: &Arc<StationRuntime>, entries: Vec<PathBuf>) -> Result<ApplyOutcome> {
    let cfg = &rt.cfg;
    let mut schedule = ScheduleFile::load(&cfg.schedule_path)?.unwrap_or_default();
    let epoch = schedule
        .epoch_unix
        .map(unix_to_time)
        .unwrap_or_else(SystemTime::now);
    let paused_at = schedule.paused_at_unix.map(unix_to_time);

    let durations = scan::durations_for(entries.clone(), &cfg.cache_file()).await;
    let (playable, unreachable) = reconcile(entries.clone(), durations);

    let station = Station::new_with(playable, epoch, paused_at);
    let outcome = ApplyOutcome {
        files: station.entries.len(),
        total_secs: station.total.as_secs_f64(),
        unreachable: unreachable.clone(),
        empty: station.is_empty(),
    };

    schedule.epoch_unix = Some(time_to_unix(epoch));
    schedule.paused_at_unix = paused_at.map(time_to_unix);
    schedule.entries = entries.into_iter().map(|path| EntryLine { path }).collect();
    schedule.save(&cfg.schedule_path)?;
    rt.swap_station(Arc::new(station)).await;

    if !unreachable.is_empty() && !outcome.empty {
        warn!(
            "{} scheduled file(s) unreachable; they stay in the schedule but will be skipped",
            unreachable.len()
        );
        let limit = if cfg.debug { unreachable.len() } else { 5 };
        for path in unreachable.iter().take(limit) {
            warn!("  unreachable: {}", path.display());
        }
        if !cfg.debug && unreachable.len() > limit {
            warn!(
                "  ... and {} more (run with --debug to list all)",
                unreachable.len() - limit
            );
        }
    }
    info!(
        "schedule applied: {} playable, {:.1} h of programming",
        outcome.files,
        outcome.total_secs / 3600.0
    );
    Ok(outcome)
}

fn reconcile(
    entries: Vec<PathBuf>,
    durations: Vec<(PathBuf, Duration)>,
) -> (Vec<(PathBuf, Duration)>, Vec<PathBuf>) {
    let mut lookup: HashMap<PathBuf, Duration> = durations.into_iter().collect();
    let mut playable = Vec::with_capacity(entries.len());
    let mut unreachable = Vec::new();
    for path in entries {
        match lookup.remove(&path) {
            Some(duration) => playable.push((path, duration)),
            None => unreachable.push(path),
        }
    }
    (playable, unreachable)
}

/// The schedule has no playable videos: shut down cleanly and exit. Used both
/// at startup and when a runtime edit/rescan empties the station.
pub async fn fatal_empty_schedule(rt: &StationRuntime, unreachable: &[PathBuf]) -> ! {
    warn!("schedule is EMPTY: no playable videos");
    if !unreachable.is_empty() {
        let limit = if rt.cfg.debug { unreachable.len() } else { 5 };
        warn!(
            "last {} file(s) that could not be reached:",
            limit.min(unreachable.len())
        );
        for path in unreachable.iter().rev().take(limit).collect::<Vec<_>>().into_iter().rev() {
            warn!("  unreachable: {}", path.display());
        }
        if !rt.cfg.debug && unreachable.len() > limit {
            warn!(
                "  ... and {} more (run with --debug to list all)",
                unreachable.len() - limit
            );
        }
    }
    warn!("fix the schedule via /admin, or run with --admin-only to edit it while the server stays up");
    let _ = rt.shutdown.send(true);
    tokio::time::sleep(Duration::from_millis(250)).await;
    std::process::exit(1);
}

/// Give the HTTP handler time to answer before the process goes down.
pub fn exit_if_empty(rt: &Arc<StationRuntime>, outcome: &ApplyOutcome) {
    if !outcome.empty {
        return;
    }
    if rt.cfg.admin_only {
        warn!("schedule is empty but --admin-only keeps the server up so you can fix it");
        return;
    }
    let rt = rt.clone();
    let unreachable = outcome.unreachable.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        fatal_empty_schedule(&rt, &unreachable).await;
    });
}

fn time_to_unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn unix_to_time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("streamstation-test-{nanos}-{name}"))
    }

    #[test]
    fn schedule_roundtrip() {
        let path = temp_file("schedule.toml");
        let schedule = ScheduleFile {
            epoch_unix: Some(1_700_000_000),
            paused_at_unix: Some(1_700_000_100),
            entries: vec![
                EntryLine {
                    path: PathBuf::from("/a/b.mp4"),
                },
                EntryLine {
                    path: PathBuf::from("/c/d.mp4"),
                },
            ],
        };
        schedule.save(&path).unwrap();
        let loaded = ScheduleFile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.epoch_unix, Some(1_700_000_000));
        assert_eq!(loaded.paused_at_unix, Some(1_700_000_100));
        assert_eq!(loaded.paths(), vec![
            PathBuf::from("/a/b.mp4"),
            PathBuf::from("/c/d.mp4")
        ]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_schedule_loads_as_none() {
        let path = temp_file("missing.toml");
        assert!(ScheduleFile::load(&path).unwrap().is_none());
    }

    #[test]
    fn reconcile_keeps_order_and_marks_unreachable() {
        let entries = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/missing.mp4"),
            PathBuf::from("/b.mp4"),
        ];
        let durations = vec![
            (PathBuf::from("/b.mp4"), Duration::from_secs(20)),
            (PathBuf::from("/a.mp4"), Duration::from_secs(10)),
        ];
        let (playable, unreachable) = reconcile(entries, durations);
        assert_eq!(
            playable,
            vec![
                (PathBuf::from("/a.mp4"), Duration::from_secs(10)),
                (PathBuf::from("/b.mp4"), Duration::from_secs(20)),
            ]
        );
        assert_eq!(unreachable, vec![PathBuf::from("/missing.mp4")]);
    }

    #[test]
    fn seek_reanchors_epoch() {
        let items = vec![
            (PathBuf::from("/a.mp4"), Duration::from_secs(100)),
            (PathBuf::from("/b.mp4"), Duration::from_secs(50)),
        ];
        let station = Station::new(items, SystemTime::UNIX_EPOCH);
        let now = UNIX_EPOCH + Duration::from_secs(30);

        // While sitting at 30s of movie 0, seek to 10s of movie 1.
        let target = station.entries[1].start + Duration::from_secs(10);
        let epoch = reanchored_epoch(target, now);
        let seeked = Station::new(
            station
                .entries
                .iter()
                .map(|e| (e.path.clone(), e.duration))
                .collect(),
            epoch,
        );

        let pb = seeked.position_at(now).unwrap();
        assert_eq!(pb.index, 1);
        assert_eq!(pb.offset, Duration::from_secs(10));

        // And it keeps advancing from there.
        let pb = seeked
            .position_at(now + Duration::from_secs(5))
            .unwrap();
        assert_eq!(pb.index, 1);
        assert_eq!(pb.offset, Duration::from_secs(15));
    }

    #[test]
    fn resume_continues_from_frozen_position() {
        let items = vec![
            (PathBuf::from("/a.mp4"), Duration::from_secs(100)),
            (PathBuf::from("/b.mp4"), Duration::from_secs(50)),
        ];
        let epoch = SystemTime::UNIX_EPOCH;
        let station = Station::new(items, epoch);

        // Paused 30s in, resumed an hour later: playback continues at 30s.
        let paused_at = UNIX_EPOCH + Duration::from_secs(30);
        let now = UNIX_EPOCH + Duration::from_secs(3600);
        assert_eq!(
            frozen_elapsed(epoch, paused_at, station.total),
            Duration::from_secs(30)
        );

        let new_epoch = resume_epoch(epoch, paused_at, now, station.total);
        let resumed = Station::new(
            station
                .entries
                .iter()
                .map(|e| (e.path.clone(), e.duration))
                .collect(),
            new_epoch,
        );
        let pb = resumed.position_at(now).unwrap();
        assert_eq!(pb.index, 0);
        assert_eq!(pb.offset, Duration::from_secs(30));
    }

    #[test]
    fn reconcile_all_missing_is_empty() {
        let entries = vec![PathBuf::from("/gone.mp4")];
        let (playable, unreachable) = reconcile(entries, Vec::new());
        assert!(playable.is_empty());
        assert_eq!(unreachable, vec![PathBuf::from("/gone.mp4")]);
    }
}
