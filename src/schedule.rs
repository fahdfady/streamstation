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
    let playable: HashMap<PathBuf, Duration> = station
        .entries
        .iter()
        .map(|e| (e.path.clone(), e.duration))
        .collect();

    let entries = schedule
        .paths()
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let duration = playable.get(&path).copied();
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ScheduleEntryView {
                index,
                path,
                title,
                duration_secs: duration.map(|d| d.as_secs_f64()),
                reachable: duration.is_some(),
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

async fn apply_locked(rt: &Arc<StationRuntime>, entries: Vec<PathBuf>) -> Result<ApplyOutcome> {
    let cfg = &rt.cfg;
    let mut schedule = ScheduleFile::load(&cfg.schedule_path)?.unwrap_or_default();
    let epoch = schedule
        .epoch_unix
        .map(unix_to_time)
        .unwrap_or_else(SystemTime::now);

    let durations = scan::durations_for(entries.clone(), &cfg.cache_file()).await;
    let (playable, unreachable) = reconcile(entries.clone(), durations);

    let station = Station::new(playable, epoch);
    let outcome = ApplyOutcome {
        files: station.entries.len(),
        total_secs: station.total.as_secs_f64(),
        unreachable: unreachable.clone(),
        empty: station.is_empty(),
    };

    schedule.epoch_unix = Some(time_to_unix(epoch));
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
    fn reconcile_all_missing_is_empty() {
        let entries = vec![PathBuf::from("/gone.mp4")];
        let (playable, unreachable) = reconcile(entries, Vec::new());
        assert!(playable.is_empty());
        assert_eq!(unreachable, vec![PathBuf::from("/gone.mp4")]);
    }
}
