use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub title: String,
    pub duration: Duration,
    pub start: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct Playback {
    pub index: usize,
    pub offset: Duration,
}

#[derive(Debug, Clone)]
pub struct Station {
    pub epoch: SystemTime,
    pub entries: Vec<Entry>,
    pub total: Duration,
}

impl Station {
    pub fn new(items: Vec<(PathBuf, Duration)>, epoch: SystemTime) -> Self {
        let mut entries = Vec::with_capacity(items.len());
        let mut acc = Duration::ZERO;
        for (path, duration) in items {
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            entries.push(Entry {
                path,
                title,
                duration,
                start: acc,
            });
            acc += duration;
        }
        Station {
            epoch,
            entries,
            total: acc,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() || self.total.is_zero()
    }

    /// The heart of the station: playback position is pure arithmetic on wall-clock
    /// time. No decoding happens until somebody actually watches.
    pub fn position_at(&self, now: SystemTime) -> Option<Playback> {
        if self.is_empty() {
            return None;
        }
        let elapsed = now.duration_since(self.epoch).unwrap_or(Duration::ZERO);
        let pos = Duration::from_nanos((elapsed.as_nanos() % self.total.as_nanos()) as u64);
        let idx = self
            .entries
            .partition_point(|e| e.start <= pos)
            .saturating_sub(1);
        Some(Playback {
            index: idx,
            offset: pos - self.entries[idx].start,
        })
    }

    pub fn next_index(&self, index: usize) -> Option<usize> {
        if self.entries.is_empty() {
            None
        } else {
            Some((index + 1) % self.entries.len())
        }
    }
}

/// Stable "random" order: every file is placed by a hash of (seed, path), so the
/// order is identical across restarts and clients, and adding a new file never
/// reorders the existing ones.
pub fn deterministic_order<T>(items: Vec<(PathBuf, T)>, seed: u64) -> Vec<(PathBuf, T)> {
    let mut keyed: Vec<(u64, PathBuf, T)> = items
        .into_iter()
        .map(|(path, value)| (fnv1a(seed, path.to_string_lossy().as_bytes()), path, value))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    keyed
        .into_iter()
        .map(|(_, path, value)| (path, value))
        .collect()
}

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325 ^ seed;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station() -> Station {
        let items = vec![
            (PathBuf::from("/a.mp4"), Duration::from_secs(10)),
            (PathBuf::from("/b.mp4"), Duration::from_secs(20)),
            (PathBuf::from("/c.mp4"), Duration::from_secs(30)),
        ];
        Station::new(items, SystemTime::UNIX_EPOCH)
    }

    #[test]
    fn position_inside_first_movie() {
        let s = station();
        let pb = s.position_at(SystemTime::UNIX_EPOCH + Duration::from_secs(5)).unwrap();
        assert_eq!(pb.index, 0);
        assert_eq!(pb.offset, Duration::from_secs(5));
    }

    #[test]
    fn position_boundaries() {
        let s = station();
        let pb = s
            .position_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        assert_eq!(pb.index, 1);
        assert_eq!(pb.offset, Duration::ZERO);

        let pb = s
            .position_at(SystemTime::UNIX_EPOCH + Duration::from_millis(29_999))
            .unwrap();
        assert_eq!(pb.index, 1);
        assert_eq!(pb.offset, Duration::from_millis(19_999));

        let pb = s
            .position_at(SystemTime::UNIX_EPOCH + Duration::from_secs(30))
            .unwrap();
        assert_eq!(pb.index, 2);
        assert_eq!(pb.offset, Duration::ZERO);
    }

    #[test]
    fn position_wraps_around() {
        let s = station();
        let pb = s
            .position_at(SystemTime::UNIX_EPOCH + Duration::from_secs(61))
            .unwrap();
        assert_eq!(pb.index, 0);
        assert_eq!(pb.offset, Duration::from_secs(1));
    }

    #[test]
    fn position_before_epoch_clamps() {
        let s = station();
        let pb = s
            .position_at(SystemTime::UNIX_EPOCH - Duration::from_secs(5))
            .unwrap();
        assert_eq!(pb.index, 0);
        assert_eq!(pb.offset, Duration::ZERO);
    }

    #[test]
    fn empty_station_has_no_position() {
        let s = Station::new(Vec::new(), SystemTime::UNIX_EPOCH);
        assert!(s.position_at(SystemTime::UNIX_EPOCH).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn order_is_stable_and_additive() {
        let seed = 7;
        let paths: Vec<PathBuf> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let items: Vec<(PathBuf, ())> = paths.iter().cloned().map(|p| (p, ())).collect();
        let ordered = deterministic_order(items.clone(), seed);
        let ordered_again = deterministic_order(items, seed);
        assert_eq!(ordered, ordered_again);

        let mut extended: Vec<(PathBuf, ())> = paths.iter().cloned().map(|p| (p, ())).collect();
        extended.push((PathBuf::from("zz-new"), ()));
        let ordered_extended = deterministic_order(extended, seed);
        let filtered: Vec<&PathBuf> = ordered_extended
            .iter()
            .map(|(p, _)| p)
            .filter(|p| p.as_os_str() != "zz-new")
            .collect();
        let original: Vec<&PathBuf> = ordered.iter().map(|(p, _)| p).collect();
        assert_eq!(filtered, original);
    }
}
