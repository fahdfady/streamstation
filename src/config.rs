use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub shuffle_seed: u64,
    pub path: PathBuf,
    pub extensions: Vec<String>,
    pub grace_secs: u64,
    pub hls_time: u64,
    pub temp_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "127.0.0.1".to_string(),
            port: 8080,
            shuffle_seed: 12345,
            path: PathBuf::from("~/Videos/Movies"),
            extensions: default_extensions(),
            grace_secs: 15,
            hls_time: 2,
            temp_dir: std::env::temp_dir().join("streamstation"),
            cache_dir: default_cache_dir(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let mut cfg = match explicit {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config {}", path.display()))?;
                toml::from_str(&text)
                    .with_context(|| format!("parsing config {}", path.display()))?
            }
            None => {
                let default_path = default_config_path();
                if default_path.exists() {
                    let text = std::fs::read_to_string(&default_path)
                        .with_context(|| format!("reading config {}", default_path.display()))?;
                    toml::from_str(&text)
                        .with_context(|| format!("parsing config {}", default_path.display()))?
                } else {
                    Config::default()
                }
            }
        };

        cfg.path = expand_tilde(&cfg.path);
        cfg.temp_dir = expand_tilde(&cfg.temp_dir);
        cfg.cache_dir = expand_tilde(&cfg.cache_dir);
        cfg.extensions = cfg
            .extensions
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        Ok(cfg)
    }

    pub fn grace(&self) -> Duration {
        Duration::from_secs(self.grace_secs)
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

fn default_extensions() -> Vec<String> {
    [
        "mp4", "mkv", "avi", "webm", "mov", "m4v", "ts", "mpg", "mpeg", "flv", "wmv", "ogv",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        })
        .join("streamstation")
        .join("config.toml")
}

fn default_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".cache")
        })
        .join("streamstation")
}
