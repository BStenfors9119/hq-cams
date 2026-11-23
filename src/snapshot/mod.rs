//! Snapshots: "Take a Pic" saves the next live frame as a JPEG.
//!
//! The frame is already a JPEG off the [`FrameBus`], so a snapshot costs no
//! capture or encode work — it's the stream's resolution, not full sensor.
//! Files follow the project's media convention:
//! `<dir>/YYYY-MM-DD/<n>.jpg`.
//!
//! Unlike the video recorder, snapshots may live on the SD card (the default
//! dir is `/var/lib/hqcam/snapshots`, so a porch cam without a USB drive still
//! works). They're small, and the folder is capped at `HQCAM_SNAPSHOT_MAX_MB`
//! by pruning the oldest, so they can never fill the OS disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cam::FrameBus;
use crate::recorder::ymd;

const DEFAULT_DIR: &str = "/var/lib/hqcam/snapshots";
const DEFAULT_MAX_MB: u64 = 200;

/// How long to wait for a fresh frame before giving up (capture stalled).
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// One saved snapshot, as the API reports it.
#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// The day folder, `YYYY-MM-DD` (UTC).
    pub date: String,
    /// The file name, `<n>.jpg`.
    pub name: String,
    pub bytes: u64,
    /// Unix seconds the file was written.
    pub taken: u64,
    /// Where to fetch the image from this server.
    pub url: String,
}

#[derive(Debug)]
pub enum SnapshotError {
    /// No frame arrived in time — the camera isn't producing frames.
    NoFrame,
    Io(io::Error),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFrame => write!(f, "the camera isn't sending pictures right now"),
            Self::Io(e) => write!(f, "couldn't save the picture: {e}"),
        }
    }
}

pub struct Snapshots {
    dir: PathBuf,
    max_bytes: u64,
    /// Serialises take/delete so two clicks can't pick the same number.
    lock: Mutex<()>,
}

impl Snapshots {
    /// `HQCAM_SNAPSHOT_DIR` (default `/var/lib/hqcam/snapshots`) and
    /// `HQCAM_SNAPSHOT_MAX_MB` (default 200).
    pub fn from_env() -> Arc<Self> {
        let dir: PathBuf = std::env::var("HQCAM_SNAPSHOT_DIR").unwrap_or_else(|_| DEFAULT_DIR.into()).into();
        let max_mb = std::env::var("HQCAM_SNAPSHOT_MAX_MB")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_MB);
        log::info!("snapshots: {} (keeps the newest {max_mb} MB)", dir.display());
        Arc::new(Self::new(dir, max_mb))
    }

    fn new(dir: PathBuf, max_mb: u64) -> Self {
        Self { dir, max_bytes: max_mb * 1024 * 1024, lock: Mutex::new(()) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Save the next live frame. Prunes the oldest snapshots first if the
    /// folder is over its cap.
    pub fn take(&self, bus: &FrameBus) -> Result<Snapshot, SnapshotError> {
        let frame = bus.next_frame(FRAME_TIMEOUT).ok_or(SnapshotError::NoFrame)?;
        self.save(&frame).map_err(SnapshotError::Io)
    }

    fn save(&self, jpeg: &[u8]) -> io::Result<Snapshot> {
        let _guard = self.lock.lock().unwrap();
        let (y, m, d) = ymd(now_secs());
        let date = format!("{y:04}-{m:02}-{d:02}");
        let day = self.dir.join(&date);
        fs::create_dir_all(&day)?;
        let name = format!("{}.jpg", next_index(&day));
        let path = day.join(&name);
        // Write under a temp name and rename, so a listing never shows half a file.
        let tmp = day.join(format!(".{name}.part"));
        fs::write(&tmp, jpeg)?;
        fs::rename(&tmp, &path)?;
        log::info!("snapshot: saved {}", path.display());
        self.prune();
        Ok(entry(&date, &name, &path).expect("file was just written"))
    }

    /// All snapshots, newest first.
    pub fn list(&self) -> Vec<Snapshot> {
        let mut all = Vec::new();
        for date in day_folders(&self.dir) {
            for (_, name) in files_in(&self.dir.join(&date)) {
                if let Some(s) = entry(&date, &name, &self.dir.join(&date).join(&name)) {
                    all.push(s);
                }
            }
        }
        all.sort_by(|a, b| (&b.date, index_of(&b.name)).cmp(&(&a.date, index_of(&a.name))));
        all
    }

    /// The file for `date`/`name`, if both are well-formed and it exists.
    /// Validation means a request can't reach outside the snapshot folder.
    pub fn path_of(&self, date: &str, name: &str) -> Option<PathBuf> {
        if !is_date(date) || index_of(name).is_none() {
            return None;
        }
        let p = self.dir.join(date).join(name);
        p.is_file().then_some(p)
    }

    /// Delete one snapshot. `Ok(false)` if it didn't exist.
    pub fn delete(&self, date: &str, name: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap();
        let Some(p) = self.path_of(date, name) else {
            return Ok(false);
        };
        fs::remove_file(&p)?;
        remove_if_empty(&self.dir.join(date));
        log::info!("snapshot: deleted {}", p.display());
        Ok(true)
    }

    /// Delete the oldest snapshots until the folder is within its cap.
    fn prune(&self) {
        let mut files: Vec<(String, u64, String, u64)> = Vec::new(); // date, index, name, bytes
        for date in day_folders(&self.dir) {
            for (i, name) in files_in(&self.dir.join(&date)) {
                let bytes = fs::metadata(self.dir.join(&date).join(&name)).map(|m| m.len()).unwrap_or(0);
                files.push((date.clone(), i, name, bytes));
            }
        }
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.3).sum();
        for (date, _, name, bytes) in files {
            if total <= self.max_bytes {
                break;
            }
            let day = self.dir.join(&date);
            if fs::remove_file(day.join(&name)).is_ok() {
                log::info!("snapshot: over the size cap — pruned {date}/{name}");
                total -= bytes;
                remove_if_empty(&day);
            }
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn entry(date: &str, name: &str, path: &Path) -> Option<Snapshot> {
    let meta = fs::metadata(path).ok()?;
    let taken = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some(Snapshot {
        date: date.into(),
        name: name.into(),
        bytes: meta.len(),
        taken,
        url: format!("/snapshots/{date}/{name}"),
    })
}

/// `YYYY-MM-DD` with digits in the right places.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// The `n` of an `<n>.jpg` name.
fn index_of(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".jpg")?;
    if stem.is_empty() || !stem.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

fn day_folders(dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_date(n))
        .collect()
}

/// `(index, name)` of every `<n>.jpg` in a day folder.
fn files_in(day: &Path) -> Vec<(u64, String)> {
    let Ok(rd) = fs::read_dir(day) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| index_of(&n).map(|i| (i, n)))
        .collect()
}

fn next_index(day: &Path) -> u64 {
    files_in(day).iter().map(|(i, _)| *i).max().unwrap_or(0) + 1
}

fn remove_if_empty(day: &Path) {
    if fs::read_dir(day).map(|mut r| r.next().is_none()).unwrap_or(false) {
        let _ = fs::remove_dir(day);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str, max_mb: u64) -> Snapshots {
        let dir = std::env::temp_dir().join(format!("hqcam-snap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Snapshots::new(dir, max_mb)
    }

    #[test]
    fn names_are_validated() {
        assert!(is_date("2026-11-22"));
        assert!(!is_date("2026-11-2x"));
        assert!(!is_date("../../etc"));
        assert_eq!(index_of("12.jpg"), Some(12));
        for bad in ["x.jpg", ".jpg", "1.jpeg", "../1.jpg", "+1.jpg", ".1.jpg.part"] {
            assert_eq!(index_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn saves_numbered_files_lists_newest_first_and_deletes() {
        let s = temp_store("basic", 200);
        let a = s.save(b"one").unwrap();
        let b = s.save(b"two").unwrap();
        assert_eq!((a.name.as_str(), b.name.as_str()), ("1.jpg", "2.jpg"));
        assert_eq!(a.url, format!("/snapshots/{}/1.jpg", a.date));
        let names: Vec<_> = s.list().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["2.jpg", "1.jpg"]);
        assert!(s.path_of(&a.date, "1.jpg").is_some());
        assert!(s.path_of("..", "1.jpg").is_none());
        assert!(s.delete(&a.date, "1.jpg").unwrap());
        assert!(!s.delete(&a.date, "1.jpg").unwrap());
        assert!(s.delete(&a.date, "2.jpg").unwrap());
        assert!(s.list().is_empty());
        assert!(!s.dir.join(&a.date).exists(), "empty day folder removed");
        fs::remove_dir_all(&s.dir).unwrap();
    }

    #[test]
    fn prunes_oldest_over_the_cap() {
        let mut s = temp_store("prune", 0);
        s.max_bytes = 10;
        s.save(b"123456").unwrap();
        s.save(b"abcdef").unwrap(); // 12 bytes > 10: 1.jpg goes
        let names: Vec<_> = s.list().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["2.jpg"]);
        fs::remove_dir_all(&s.dir).unwrap();
    }
}
