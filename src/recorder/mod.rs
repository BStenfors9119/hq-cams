//! Offline footage recorder.
//!
//! A second consumer of the [`FrameBus`] (alongside the HTTP `MjpegStream`).
//! While the camera is *off* the HQ network — so Motion Eye can't pull and
//! store the feed — this writes the live JPEG frames to an attached USB drive
//! so the footage isn't lost. Back on HQ WiFi it stops and lets HQ own
//! recording again.
//!
//! Design choices for a Pi Zero in a vehicle (abrupt power loss is normal):
//! - **Raw MJPEG** files (concatenated JPEGs straight off the bus). No
//!   re-encoding — the frames are already hardware-JPEG — and no container to
//!   finalize, so a power cut truncates only the final frame; everything before
//!   it stays valid and playable. VLC/ffmpeg read raw `.mjpeg` directly, and it
//!   transcodes losslessly later with `ffmpeg -c copy`.
//! - **Date folders, numbered segments** (`<mount>/YYYY-MM-DD/1.mjpeg`) per the
//!   project's media convention, rotated every few minutes.
//! - **Never writes to the SD card**: recording only starts when the target is
//!   a real mountpoint, so a missing/unmounted drive can't quietly fill the OS
//!   disk and take the camera down with it.
//! - **Ring buffer**: prunes the oldest segments when the drive runs low.
//!
//! Everything is tunable via env vars (see [`Config`]), wired through the
//! service's optional `EnvironmentFile` at `/usr/local/etc/hqcam/record.conf`.
//!
//! Besides the automatic offline mode there is a **manual** switch
//! ([`Recorder::set_manual`], the web UI's Record button): it records
//! continuously, online or not, until switched off. It obeys the same
//! never-the-SD-card rule — no USB drive, no manual recording.

mod motion;

use std::collections::VecDeque;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cam::FrameBus;
use motion::{MotionDetector, MotionOpts};

/// How often an idle recorder checks whether the manual switch was flipped.
const MANUAL_POLL: Duration = Duration::from_millis(250);

/// Capture frame rate — keep in sync with `--framerate` in `cam::RPICAM_ARGS`.
/// Used only to size the motion pre-roll buffer in frames.
const CAPTURE_FPS: u32 = 15;

/// Hard cap on the pre-roll buffer regardless of configured seconds, so a
/// misconfiguration can't exhaust memory on a Pi Zero.
const PREROLL_MAX_BYTES: usize = 24 * 1024 * 1024;

/// Recorder tunables, all overridable by environment variable.
struct Config {
    /// Automatic offline recording on (`HQCAM_RECORD`, default on). Manual
    /// recording works either way.
    auto: bool,
    /// USB mount to record onto. MUST be a real mountpoint or recording is
    /// skipped (we never write footage to the SD card).
    dir: PathBuf,
    /// Length of a single segment file before rotating to the next number.
    segment: Duration,
    /// Keep at least this much free on the drive; prune oldest segments below it.
    min_free_mb: u64,
    /// WiFi interface and AP profile name — used to tell "on HQ WiFi" (a client
    /// connection that isn't our own hotspot) from "offline". Kept in step with
    /// the hotspot script's defaults.
    iface: String,
    ap_con: String,
    /// How often to re-evaluate online/offline while idle or mid-segment.
    check: Duration,
    /// Record only when motion is detected, instead of continuously.
    motion_gated: bool,
    /// Motion analysis tuning.
    motion: MotionOpts,
    /// Keep recording this long after motion stops (post-roll), so a clip isn't
    /// cut off the instant a subject pauses.
    motion_timeout: Duration,
    /// Seconds of footage kept before a motion trigger, prepended to the clip so
    /// the lead-up isn't lost. Motion is only noticed *after* it starts.
    preroll: Duration,
}

impl Config {
    fn from_env() -> Self {
        Self {
            auto: env_bool("HQCAM_RECORD", true),
            dir: env::var("HQCAM_RECORD_DIR")
                .unwrap_or_else(|_| "/media/hqcam".into())
                .into(),
            segment: Duration::from_secs(env_u64("HQCAM_SEGMENT_SECS", 300)),
            min_free_mb: env_u64("HQCAM_MIN_FREE_MB", 512),
            iface: env::var("HQCAM_WIFI_IFACE").unwrap_or_else(|_| "wlan0".into()),
            ap_con: env::var("HQCAM_AP_CON").unwrap_or_else(|_| "hqcam-ap".into()),
            check: Duration::from_secs(env_u64("HQCAM_CHECK_SECS", 5).max(1)),
            motion_gated: env_bool("HQCAM_MOTION_GATED", false),
            motion: MotionOpts {
                fps: env_u64("HQCAM_MOTION_FPS", 3) as u32,
                sensitivity: env_f32("HQCAM_MOTION_SENSITIVITY", 0.02),
                cell_threshold: env_u64("HQCAM_MOTION_CELL_THRESHOLD", 18).min(255) as u8,
            },
            motion_timeout: Duration::from_secs(env_u64("HQCAM_MOTION_TIMEOUT", 10)),
            preroll: Duration::from_secs(env_u64("HQCAM_PREROLL_SECS", 3)),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_f32(key: &str, default: f32) -> f32 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key).ok().as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("on") => true,
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => default,
    }
}

/// Shared handle to the recorder: the manual switch and what it's doing now.
pub struct Recorder {
    manual: AtomicBool,
    auto: bool,
    motion_gated: bool,
    dir: PathBuf,
    /// The file being written right now, if any.
    current: Mutex<Option<PathBuf>>,
}

/// What the recorder is doing, as the API reports it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatus {
    /// The manual Record switch.
    pub manual: bool,
    /// Automatic recording while off HQ WiFi (`HQCAM_RECORD`).
    pub auto: bool,
    /// `continuous` or `motion` — how automatic recording captures.
    pub auto_mode: &'static str,
    /// A file is being written right now.
    pub recording: bool,
    pub file: Option<String>,
    pub dir: String,
    pub drive_mounted: bool,
    pub free_mb: Option<u64>,
}

#[derive(Debug)]
pub enum RecordError {
    /// Manual recording needs the USB drive; it never writes to the SD card.
    NoDrive(PathBuf),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDrive(dir) => write!(f, "no USB drive is plugged in (looked at {})", dir.display()),
        }
    }
}

impl Recorder {
    pub fn manual(&self) -> bool {
        self.manual.load(Ordering::Relaxed)
    }

    /// Flip the manual Record switch. Turning it on needs the USB drive.
    pub fn set_manual(&self, on: bool) -> Result<(), RecordError> {
        if on && !mount_ready(&self.dir) {
            return Err(RecordError::NoDrive(self.dir.clone()));
        }
        self.manual.store(on, Ordering::Relaxed);
        log::info!("recorder: manual recording {}", if on { "on" } else { "off" });
        Ok(())
    }

    pub fn status(&self) -> RecorderStatus {
        let file = self.current.lock().unwrap().as_ref().map(|p| p.display().to_string());
        let drive_mounted = mount_ready(&self.dir);
        RecorderStatus {
            manual: self.manual(),
            auto: self.auto,
            auto_mode: if self.motion_gated { "motion" } else { "continuous" },
            recording: file.is_some(),
            file,
            dir: self.dir.display().to_string(),
            drive_mounted,
            free_mb: if drive_mounted { free_mb(&self.dir) } else { None },
        }
    }

    fn set_current(&self, path: Option<PathBuf>) {
        *self.current.lock().unwrap() = path;
    }
}

/// Start the recorder thread and return its handle. Always started (the manual
/// switch needs it); automatic offline recording self-gates on being offline
/// with a mounted drive, and `HQCAM_RECORD=0` turns only that part off.
pub fn start(bus: Arc<FrameBus>) -> Arc<Recorder> {
    let cfg = Config::from_env();
    let rec = Arc::new(Recorder {
        manual: AtomicBool::new(false),
        auto: cfg.auto,
        motion_gated: cfg.motion_gated,
        dir: cfg.dir.clone(),
        current: Mutex::new(None),
    });
    if cfg.auto {
        let mode = if cfg.motion_gated { "motion-gated" } else { "continuous" };
        log::info!(
            "recorder: automatic ({mode}) — dir={}, segment={}s, keep-free={}MB (records only when off HQ WiFi with a USB drive mounted)",
            cfg.dir.display(),
            cfg.segment.as_secs(),
            cfg.min_free_mb,
        );
    } else {
        log::info!("recorder: automatic recording disabled (HQCAM_RECORD=0); manual Record still works");
    }
    let handle = rec.clone();
    thread::spawn(move || run(bus, cfg, handle));
    rec
}

/// Sleep for up to `cfg.check`, waking early if the manual switch turns on.
fn idle(cfg: &Config, rec: &Recorder) {
    let start = Instant::now();
    while start.elapsed() < cfg.check {
        if rec.manual() {
            return;
        }
        thread::sleep(MANUAL_POLL);
    }
}

/// Whether to keep recording, re-checking the (slow, `nmcli`) offline test at
/// most every `cfg.check`. Manual on => record regardless of the network.
struct Gate {
    offline: bool,
    last_check: Instant,
}

impl Gate {
    fn new(cfg: &Config) -> Self {
        Self { offline: cfg.auto && is_offline(cfg), last_check: Instant::now() }
    }

    /// Automatic recording wanted right now (auto mode + off HQ WiFi).
    fn auto_wanted(&mut self, cfg: &Config) -> bool {
        if !cfg.auto {
            return false;
        }
        if self.last_check.elapsed() >= cfg.check {
            self.offline = is_offline(cfg);
            self.last_check = Instant::now();
        }
        self.offline
    }
}

fn run(bus: Arc<FrameBus>, cfg: Config, rec: Arc<Recorder>) {
    let mut last_seq = 0u64;
    let mut warned_no_drive = false;

    loop {
        let manual = rec.manual();
        let auto_now = !manual && cfg.auto && is_offline(&cfg);
        if !manual && !auto_now {
            idle(&cfg, &rec);
            continue;
        }
        if !mount_ready(&cfg.dir) {
            if manual {
                log::error!("recorder: USB drive gone — manual recording stopped");
                rec.manual.store(false, Ordering::Relaxed);
            } else if !warned_no_drive {
                log::warn!(
                    "recorder: off HQ WiFi but no USB drive mounted at {} — footage is NOT being saved",
                    cfg.dir.display()
                );
                warned_no_drive = true;
            }
            idle(&cfg, &rec);
            continue;
        }
        warned_no_drive = false;

        // Manual recording is always continuous; automatic follows HQCAM_MOTION_GATED.
        let session = if manual || !cfg.motion_gated {
            record_continuous_segment(&bus, &cfg, &rec, &mut last_seq)
        } else {
            record_motion_session(&bus, &cfg, &rec, &mut last_seq)
        };
        rec.set_current(None);
        if let Err(e) = session {
            // Most likely the drive was pulled mid-write; back off and re-check.
            log::error!("recorder: segment ended early: {e}");
            thread::sleep(cfg.check);
        }
    }
}

/// One open segment being written, with its own rotation clock and periodic
/// flush so a power cut loses at most ~1s of footage.
///
/// It's written to a `.mjpeg.part` file and atomically renamed to `.mjpeg` on
/// clean close, so the Tier-3 sync only ever sees finalized files — never one
/// still being written — and a power-cut leftover is visibly marked `.part`.
struct Segment {
    writer: BufWriter<File>,
    part_path: PathBuf,
    final_path: PathBuf,
    start: Instant,
    last_flush: Instant,
}

impl Segment {
    /// Prune to free space, then create the next numbered file in today's folder.
    fn open(cfg: &Config) -> std::io::Result<Self> {
        prune_until_free(&cfg.dir, cfg.min_free_mb);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (y, m, d) = ymd(now);
        let day_dir = cfg.dir.join(format!("{y:04}-{m:02}-{d:02}"));
        fs::create_dir_all(&day_dir)?;

        // next_index counts both .mjpeg and .mjpeg.part, so a crashed leftover
        // never gets its number reused.
        let n = next_index(&day_dir);
        let final_path = day_dir.join(format!("{n}.mjpeg"));
        let part_path = day_dir.join(format!("{n}.mjpeg.part"));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part_path)?;
        log::info!("recorder: recording to {}", final_path.display());

        let now_i = Instant::now();
        Ok(Self { writer: BufWriter::new(file), part_path, final_path, start: now_i, last_flush: now_i })
    }

    /// Append a frame, flushing about once a second (rather than per frame).
    fn write(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(frame)?;
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.writer.flush()?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Flush + fsync, then atomically rename `.part` → `.mjpeg`. Failures are
    /// logged, not propagated (a leftover `.part` is recoverable next run).
    fn close(mut self) {
        if let Err(e) = self.writer.flush() {
            log::warn!("recorder: flushing {} failed: {e}", self.part_path.display());
        }
        let _ = self.writer.get_ref().sync_all();
        drop(self.writer); // close the fd before renaming
        if let Err(e) = fs::rename(&self.part_path, &self.final_path) {
            log::warn!("recorder: finalising {} failed: {e}", self.final_path.display());
        }
    }
}

/// Continuous mode: write one full segment, returning when it fills its time
/// budget, when recording is no longer wanted (manual switched off and/or back
/// on HQ WiFi), or on the first I/O error.
fn record_continuous_segment(
    bus: &FrameBus,
    cfg: &Config,
    rec: &Recorder,
    last_seq: &mut u64,
) -> std::io::Result<()> {
    let mut seg = Segment::open(cfg)?;
    rec.set_current(Some(seg.final_path.clone()));
    let mut gate = Gate::new(cfg);

    loop {
        if seg.elapsed() >= cfg.segment {
            break; // rotate to the next segment (re-prunes on the next open)
        }
        if !rec.manual() && !gate.auto_wanted(cfg) {
            log::info!("recorder: stopping (manual off / back on HQ WiFi)");
            break;
        }

        let (frame, seq) = bus.wait_for(*last_seq);
        *last_seq = seq;
        seg.write(&frame)?;
    }

    seg.close();
    Ok(())
}

/// Motion-gated mode: analyse every frame but only write around motion. A
/// pre-roll ring prepends the seconds *before* each trigger (motion is only
/// noticed after it starts), and recording continues for `motion_timeout` after
/// motion stops. Returns when we come back online, the drive is removed, or on
/// the first I/O error.
fn record_motion_session(
    bus: &FrameBus,
    cfg: &Config,
    rec: &Recorder,
    last_seq: &mut u64,
) -> std::io::Result<()> {
    let mut detector = MotionDetector::new(&cfg.motion);
    let max_preroll = (cfg.preroll.as_secs() as usize * CAPTURE_FPS as usize).max(1);
    let mut preroll: VecDeque<Arc<Vec<u8>>> = VecDeque::new();
    let mut preroll_bytes = 0usize;
    let mut seg: Option<Segment> = None;
    let mut last_motion: Option<Instant> = None;
    let mut last_check = Instant::now();
    let mut gate = Gate::new(cfg);

    loop {
        if rec.manual() {
            break; // switch to manual (continuous) recording
        }
        if !gate.auto_wanted(cfg) {
            log::info!("recorder: back on HQ WiFi — handing recording back to HQ");
            break;
        }
        if last_check.elapsed() >= cfg.check {
            if !mount_ready(&cfg.dir) {
                break; // drive removed
            }
            last_check = Instant::now();
        }

        let (frame, seq) = bus.wait_for(*last_seq);
        *last_seq = seq;

        if let Some(true) = detector.update(&frame) {
            last_motion = Some(Instant::now());
        }
        let motion_active = last_motion.map_or(false, |t| t.elapsed() < cfg.motion_timeout);

        if let Some(s) = seg.as_mut() {
            s.write(&frame)?;
            let rotated = s.elapsed() >= cfg.segment;
            let end = rotated || !motion_active;
            // Keep pre-roll fresh for the next clip; `s` is not used past here.
            push_preroll(&mut preroll, &mut preroll_bytes, frame, max_preroll);
            if end {
                if !rotated {
                    log::info!("recorder: motion stopped — ending clip");
                }
                seg.take().unwrap().close();
                rec.set_current(None);
            }
        } else {
            push_preroll(&mut preroll, &mut preroll_bytes, frame, max_preroll);
            if motion_active {
                log::info!("recorder: motion detected — starting clip");
                let mut s = Segment::open(cfg)?;
                rec.set_current(Some(s.final_path.clone()));
                for f in preroll.drain(..) {
                    s.write(&f)?; // prepend the buffered lead-up (includes this frame)
                }
                preroll_bytes = 0;
                seg = Some(s);
            }
        }
    }

    if let Some(s) = seg {
        s.close();
    }
    Ok(())
}

/// Append a frame to the pre-roll ring, dropping the oldest to stay within both
/// the frame-count and hard byte caps.
fn push_preroll(buf: &mut VecDeque<Arc<Vec<u8>>>, bytes: &mut usize, frame: Arc<Vec<u8>>, max_frames: usize) {
    *bytes += frame.len();
    buf.push_back(frame);
    while buf.len() > max_frames || *bytes > PREROLL_MAX_BYTES {
        match buf.pop_front() {
            Some(f) => *bytes -= f.len(),
            None => break,
        }
    }
}

/// True when we are NOT connected to HQ (or any) WiFi as a client — i.e. either
/// disconnected or running our own hotspot, so nothing upstream is recording.
///
/// If `nmcli` can't be run or errors, assume offline: better to record
/// redundantly than to silently lose footage.
fn is_offline(cfg: &Config) -> bool {
    let out = match Command::new("nmcli")
        .args(["-t", "-f", "NAME,DEVICE,TYPE", "connection", "show", "--active"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return true,
    };

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // `nmcli -t` is colon-separated; only NAME can contain a literal colon,
        // so split from the right to peel off DEVICE and TYPE cleanly.
        let mut parts = line.rsplitn(3, ':');
        let typ = parts.next().unwrap_or("");
        let dev = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        if typ == "802-11-wireless" && dev == cfg.iface && name != cfg.ap_con {
            return false; // an active client WiFi that isn't our hotspot => online
        }
    }
    true
}

/// True only if `dir` is a real mountpoint (a different filesystem than its
/// parent). Guards against writing footage onto the SD card when the USB drive
/// is absent or unmounted.
pub(crate) fn mount_ready(dir: &Path) -> bool {
    let (Ok(here), Some(parent)) = (fs::metadata(dir), dir.parent()) else {
        return false;
    };
    match fs::metadata(parent) {
        Ok(up) => here.is_dir() && here.dev() != up.dev(),
        Err(_) => false,
    }
}

/// Next free `<n>.mjpeg` index in a day folder (max existing + 1, starting at 1).
fn next_index(dir: &Path) -> u64 {
    let mut max = 0u64;
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(i) = segment_index(&entry.path()) {
                max = max.max(i);
            }
        }
    }
    max + 1
}

/// Parse the numeric index out of a `<n>.mjpeg` (or in-progress `<n>.mjpeg.part`)
/// path, if it is one.
fn segment_index(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".mjpeg.part")
        .or_else(|| name.strip_suffix(".mjpeg"))?;
    stem.parse().ok()
}

/// Delete oldest segments until at least `min_free_mb` is free (ring buffer).
fn prune_until_free(base: &Path, min_free_mb: u64) {
    loop {
        match free_mb(base) {
            Some(mb) if mb >= min_free_mb => return,
            None => {
                log::warn!("recorder: can't read free space on {}; skipping prune", base.display());
                return;
            }
            Some(_) => {}
        }
        let Some(oldest) = oldest_segment(base) else {
            return; // nothing left to reclaim
        };
        log::info!("recorder: low on space — pruning {}", oldest.display());
        let _ = fs::remove_file(&oldest);
        // Drop the day folder too once its last segment is gone.
        if let Some(day) = oldest.parent() {
            if fs::read_dir(day).map(|mut r| r.next().is_none()).unwrap_or(false) {
                let _ = fs::remove_dir(day);
            }
        }
    }
}

/// The lowest-numbered segment in the earliest day folder, or `None` if empty.
fn oldest_segment(base: &Path) -> Option<PathBuf> {
    let mut days: Vec<PathBuf> = match fs::read_dir(base) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => return None,
    };
    days.sort(); // YYYY-MM-DD names sort chronologically

    for day in days {
        let rd = match fs::read_dir(&day) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let earliest = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                segment_index(&p).map(|i| (i, p))
            })
            .min_by_key(|(i, _)| *i);
        if let Some((_, path)) = earliest {
            return Some(path);
        }
    }
    None
}

/// Free megabytes on the filesystem holding `path`, via `df` (no extra crate,
/// consistent with the project shelling out to `rpicam-vid`/`nmcli`).
pub(crate) fn free_mb(path: &Path) -> Option<u64> {
    let out = Command::new("df").arg("-kP").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // POSIX `df -P` columns: Filesystem 1024-blocks Used Available Capacity Mount.
    let text = String::from_utf8_lossy(&out.stdout);
    let avail_kb: u64 = text.lines().last()?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / 1024)
}

/// UTC (year, month, day) for a Unix timestamp — Howard Hinnant's
/// civil-from-days, so no date crate is needed for folder names.
pub(crate) fn ymd(secs: u64) -> (i64, u32, u32) {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
