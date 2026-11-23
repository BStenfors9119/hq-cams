//! Snapshots: "Take a Pic" grabs the next live frame as a JPEG.
//!
//! The frame is already a JPEG off the [`FrameBus`], so a snapshot costs no
//! capture or encode work — it's the stream's resolution, not full sensor.
//!
//! **Never written to the SD card.** A picture is saved only to real external
//! storage, checked per request:
//! 1. a mounted network drive (`HQCAM_NETWORK_DIR`, unset by default) —
//!    preferred, because it's off the device (a stolen camera takes nothing
//!    with it);
//! 2. the USB drive (`HQCAM_RECORD_DIR`, default `/media/hqcam`).
//!
//! Each must be a real mountpoint ([`mount_ready`]), so an unmounted target is
//! "no storage", never a folder on the SD card. Saved files follow the media
//! convention under a `snapshots` folder: `<target>/snapshots/YYYY-MM-DD/<n>.jpg`,
//! capped at `HQCAM_SNAPSHOT_MAX_MB` by pruning the oldest.
//!
//! With no storage, a picture can still be taken **download-only**
//! ([`Snapshots::grab`]): the frame goes straight back to the device that asked
//! for it in the HTTP response, held only in memory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cam::FrameBus;
use crate::recorder::{mount_ready, ymd};

const DEFAULT_USB_DIR: &str = "/media/hqcam";
/// Sub-folder of a storage target that holds the pictures (video segments use
/// the target's top-level date folders).
const SUBDIR: &str = "snapshots";
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
    /// No network or USB drive to save to; only a download-only picture works.
    NoStorage,
    Io(io::Error),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFrame => write!(f, "the camera isn't sending pictures right now"),
            Self::NoStorage => write!(
                f,
                "no USB or network drive is connected, so pictures can't be saved on the camera — download it instead"
            ),
            Self::Io(e) => write!(f, "couldn't save the picture: {e}"),
        }
    }
}

/// Where pictures are being saved right now.
#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum StorageKind {
    Network,
    Usb,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Storage {
    pub kind: StorageKind,
    /// The folder pictures go in (`<target>/snapshots`).
    pub dir: PathBuf,
    /// The mounted drive the folder is on.
    #[serde(skip)]
    pub mount: PathBuf,
}

type ReadyFn = Box<dyn Fn(&Path) -> bool + Send + Sync>;

pub struct Snapshots {
    /// Candidate drives in order of preference.
    targets: Vec<(StorageKind, PathBuf)>,
    /// Is this target mounted? ([`mount_ready`]; swapped out in tests.)
    ready: ReadyFn,
    max_bytes: u64,
    /// Serialises save/delete so two clicks can't pick the same number.
    lock: Mutex<()>,
}

impl Snapshots {
    /// `HQCAM_NETWORK_DIR` (a mounted network share; unset = none),
    /// `HQCAM_RECORD_DIR` (the USB drive, default `/media/hqcam`) and
    /// `HQCAM_SNAPSHOT_MAX_MB` (default 200).
    pub fn from_env() -> Arc<Self> {
        let mut targets = Vec::new();
        if let Some(net) = std::env::var("HQCAM_NETWORK_DIR").ok().filter(|v| !v.trim().is_empty()) {
            targets.push((StorageKind::Network, PathBuf::from(net.trim())));
        }
        let usb = std::env::var("HQCAM_RECORD_DIR").unwrap_or_else(|_| DEFAULT_USB_DIR.into());
        targets.push((StorageKind::Usb, usb.into()));
        let max_mb = std::env::var("HQCAM_SNAPSHOT_MAX_MB")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_MB);
        let names: Vec<String> = targets.iter().map(|(k, p)| format!("{k:?} {}", p.display())).collect();
        log::info!(
            "snapshots: saved to the first mounted of [{}] (newest {max_mb} MB kept); none mounted => download-only",
            names.join(", ")
        );
        Arc::new(Self::new(targets, max_mb, Box::new(mount_ready)))
    }

    fn new(targets: Vec<(StorageKind, PathBuf)>, max_mb: u64, ready: ReadyFn) -> Self {
        Self { targets, ready, max_bytes: max_mb * 1024 * 1024, lock: Mutex::new(()) }
    }

    /// The storage pictures are saved to right now, or `None` (download-only).
    pub fn storage(&self) -> Option<Storage> {
        self.targets.iter().find(|(_, p)| (self.ready)(p)).map(|(kind, p)| Storage {
            kind: *kind,
            dir: p.join(SUBDIR),
            mount: p.clone(),
        })
    }

    /// Save the next live frame to the current storage. Fails with
    /// [`SnapshotError::NoStorage`] when there is none — use [`grab`](Self::grab).
    pub fn take(&self, bus: &FrameBus) -> Result<(Snapshot, Storage), SnapshotError> {
        let storage = self.storage().ok_or(SnapshotError::NoStorage)?;
        let frame = bus.next_frame(FRAME_TIMEOUT).ok_or(SnapshotError::NoFrame)?;
        let snap = self.save(&storage.dir, &frame).map_err(SnapshotError::Io)?;
        Ok((snap, storage))
    }

    /// Download-only: the next live frame, held in memory and never written
    /// anywhere on the camera.
    pub fn grab(&self, bus: &FrameBus) -> Result<Arc<Vec<u8>>, SnapshotError> {
        bus.next_frame(FRAME_TIMEOUT).ok_or(SnapshotError::NoFrame)
    }

    fn save(&self, dir: &Path, jpeg: &[u8]) -> io::Result<Snapshot> {
        let _guard = self.lock.lock().unwrap();
        let (y, m, d) = ymd(now_secs());
        let date = format!("{y:04}-{m:02}-{d:02}");
        let day = dir.join(&date);
        fs::create_dir_all(&day)?;
        let name = format!("{}.jpg", next_index(&day));
        let path = day.join(&name);
        // Write under a temp name and rename, so a listing never shows half a file.
        let tmp = day.join(format!(".{name}.part"));
        fs::write(&tmp, jpeg)?;
        fs::rename(&tmp, &path)?;
        log::info!("snapshot: saved {}", path.display());
        self.prune(dir);
        Ok(entry(&date, &name, &path).expect("file was just written"))
    }

    /// The pictures on the current storage, newest first (empty with none).
    pub fn list(&self) -> Vec<Snapshot> {
        let Some(storage) = self.storage() else { return Vec::new() };
        let dir = storage.dir;
        let mut all = Vec::new();
        for date in day_folders(&dir) {
            for (_, name) in files_in(&dir.join(&date)) {
                if let Some(s) = entry(&date, &name, &dir.join(&date).join(&name)) {
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
        let p = self.storage()?.dir.join(date).join(name);
        p.is_file().then_some(p)
    }

    /// Delete one snapshot. `Ok(false)` if it didn't exist.
    pub fn delete(&self, date: &str, name: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap();
        let Some(p) = self.path_of(date, name) else {
            return Ok(false);
        };
        fs::remove_file(&p)?;
        if let Some(day) = p.parent() {
            remove_if_empty(day);
        }
        log::info!("snapshot: deleted {}", p.display());
        Ok(true)
    }

    /// Delete the oldest snapshots in `dir` until it is within its cap.
    fn prune(&self, dir: &Path) {
        let mut files: Vec<(String, u64, String, u64)> = Vec::new(); // date, index, name, bytes
        for date in day_folders(dir) {
            for (i, name) in files_in(&dir.join(&date)) {
                let bytes = fs::metadata(dir.join(&date).join(&name)).map(|m| m.len()).unwrap_or(0);
                files.push((date.clone(), i, name, bytes));
            }
        }
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.3).sum();
        for (date, _, name, bytes) in files {
            if total <= self.max_bytes {
                break;
            }
            let day = dir.join(&date);
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
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hqcam-snap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A store whose single USB target is always "mounted".
    fn usb_store(tag: &str, max_mb: u64) -> (Snapshots, PathBuf) {
        let dir = temp_dir(tag);
        (Snapshots::new(vec![(StorageKind::Usb, dir.clone())], max_mb, Box::new(|_| true)), dir)
    }

    fn save(s: &Snapshots, bytes: &[u8]) -> Snapshot {
        s.save(&s.storage().unwrap().dir, bytes).unwrap()
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
    fn no_mounted_target_means_no_storage_and_nothing_written() {
        let dir = temp_dir("none");
        let s = Snapshots::new(vec![(StorageKind::Usb, dir.clone())], 200, Box::new(|_| false));
        assert_eq!(s.storage(), None);
        assert!(s.list().is_empty());
        assert!(s.path_of("2026-11-22", "1.jpg").is_none());
        let bus = FrameBus::new();
        assert!(matches!(s.take(&bus), Err(SnapshotError::NoStorage)));
        assert!(!dir.join(SUBDIR).exists(), "nothing created on an unmounted target");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prefers_the_network_drive_and_falls_back_to_usb() {
        let net = PathBuf::from("/net");
        let usb = PathBuf::from("/usb");
        let net_up = Arc::new(AtomicBool::new(true));
        let flag = net_up.clone();
        let s = Snapshots::new(
            vec![(StorageKind::Network, net.clone()), (StorageKind::Usb, usb.clone())],
            200,
            Box::new(move |p| p != Path::new("/net") || flag.load(Ordering::Relaxed)),
        );
        let st = s.storage().unwrap();
        assert_eq!((st.kind, st.dir), (StorageKind::Network, net.join(SUBDIR)));
        net_up.store(false, Ordering::Relaxed);
        let st = s.storage().unwrap();
        assert_eq!((st.kind, st.dir), (StorageKind::Usb, usb.join(SUBDIR)));
    }

    #[test]
    fn saves_numbered_files_lists_newest_first_and_deletes() {
        let (s, dir) = usb_store("basic", 200);
        let a = save(&s, b"one");
        let b = save(&s, b"two");
        assert_eq!((a.name.as_str(), b.name.as_str()), ("1.jpg", "2.jpg"));
        assert_eq!(a.url, format!("/snapshots/{}/1.jpg", a.date));
        assert!(dir.join(SUBDIR).join(&a.date).join("1.jpg").is_file());
        let names: Vec<_> = s.list().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["2.jpg", "1.jpg"]);
        assert!(s.path_of(&a.date, "1.jpg").is_some());
        assert!(s.path_of("..", "1.jpg").is_none());
        assert!(s.delete(&a.date, "1.jpg").unwrap());
        assert!(!s.delete(&a.date, "1.jpg").unwrap());
        assert!(s.delete(&a.date, "2.jpg").unwrap());
        assert!(s.list().is_empty());
        assert!(!dir.join(SUBDIR).join(&a.date).exists(), "empty day folder removed");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prunes_oldest_over_the_cap() {
        let (mut s, dir) = usb_store("prune", 0);
        s.max_bytes = 10;
        save(&s, b"123456");
        save(&s, b"abcdef"); // 12 bytes > 10: 1.jpg goes
        let names: Vec<_> = s.list().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["2.jpg"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
