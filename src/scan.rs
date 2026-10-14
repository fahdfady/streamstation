use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tracing::{info, warn};
use walkdir::WalkDir;

const PROBE_CONCURRENCY: usize = 8;

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    files: HashMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct CacheEntry {
    mtime_ms: u64,
    size: u64,
    duration_ms: u64,
}

pub async fn scan(
    dir: &Path,
    extensions: &[String],
    cache_path: &Path,
) -> Result<Vec<(PathBuf, Duration)>> {
    let dir = dir.to_path_buf();
    let dir_label = dir.display().to_string();
    let exts = extensions.to_vec();

    let mut files: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for entry in WalkDir::new(&dir).follow_links(true).into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    warn!("skipping unreadable entry: {err}");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let matches = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.contains(&e.to_ascii_lowercase()))
                .unwrap_or(false);
            if matches {
                out.push(path.to_path_buf());
            }
        }
        out.sort();
        out
    })
    .await?;

    let cache_file = load_cache(cache_path);
    let cached: Arc<HashMap<String, CacheEntry>> = Arc::new(cache_file.files);
    let semaphore = Arc::new(Semaphore::new(PROBE_CONCURRENCY));

    let results = futures_util::stream::iter(files.drain(..).map(|path| {
        let cached = cached.clone();
        let semaphore = semaphore.clone();
        async move { resolve(&path, &cached, semaphore).await }
    }))
    .buffer_unordered(PROBE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut items: Vec<(PathBuf, Duration)> = Vec::new();
    let mut fresh_cache = CacheFile::default();
    for (path, duration, entry) in results.into_iter().flatten() {
        fresh_cache
            .files
            .insert(path.to_string_lossy().into_owned(), entry);
        items.push((path, duration));
    }

    items.sort_by(|a, b| a.0.cmp(&b.0));
    save_cache(cache_path, &fresh_cache).await;
    info!("scanned {dir_label}: {} playable files", items.len());
    Ok(items)
}

async fn resolve(
    path: &Path,
    cached: &HashMap<String, CacheEntry>,
    semaphore: Arc<Semaphore>,
) -> Option<(PathBuf, Duration, CacheEntry)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let size = meta.len();

    let key = path.to_string_lossy().into_owned();
    if let Some(hit) = cached.get(&key)
        && hit.mtime_ms == mtime_ms
        && hit.size == size
    {
        return Some((
            path.to_path_buf(),
            Duration::from_millis(hit.duration_ms),
            *hit,
        ));
    }

    let _permit = semaphore.acquire().await.ok()?;
    match probe_duration(path).await {
        Ok(duration) => {
            info!("probed {} ({:.0}s)", path.display(), duration.as_secs_f64());
            Some((
                path.to_path_buf(),
                duration,
                CacheEntry {
                    mtime_ms,
                    size,
                    duration_ms: duration.as_millis() as u64,
                },
            ))
        }
        Err(err) => {
            warn!("skipping {}: {err:#}", path.display());
            None
        }
    }
}

async fn probe_duration(path: &Path) -> Result<Duration> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .await
        .context("failed to run ffprobe (is it installed?)")?;

    if !output.status.success() {
        bail!(
            "ffprobe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let secs: f64 = text.trim().parse().context("parsing ffprobe duration")?;
    if !secs.is_finite() || secs <= 0.0 {
        bail!("invalid duration {secs}");
    }
    Ok(Duration::from_secs_f64(secs))
}

fn load_cache(path: &Path) -> CacheFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

async fn save_cache(path: &Path, cache: &CacheFile) {
    let Ok(json) = serde_json::to_string(cache) else {
        return;
    };
    let Some(parent) = path.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::fs::create_dir_all(&parent);
        let _ = std::fs::write(&path, json);
    })
    .await;
}


