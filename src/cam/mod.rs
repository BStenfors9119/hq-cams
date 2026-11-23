//! CSI camera capture.
//!
//! Spawns `rpicam-vid` to produce a hardware-encoded MJPEG stream on its stdout,
//! splits that stream into individual JPEG frames, and publishes the latest
//! frame to a [`FrameBus`] that HTTP clients read from. The subprocess is
//! supervised: if it exits or fails to start, it is restarted after a delay.
//!
//! This is the Option 1 ("shell out to rpicam-vid") approach: the camera/ISP/
//! hardware-JPEG path is owned entirely by Raspberry Pi's `rpicam-vid`, so this
//! crate only does stream plumbing and never links libcamera directly.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::rotate::Rotator;
use std::thread;
use std::time::{Duration, Instant};

/// Fixed `rpicam-vid` arguments. Resolution / framerate / quality come from
/// [`Capture`] (env-configurable) and orientation from the live [`CamControl`];
/// both are appended per-spawn in [`rpicam_args`].
const RPICAM_BASE_ARGS: &[&str] = &[
    "--codec", "mjpeg",
    "--nopreview",
    "--flush", // push each frame out immediately (lower latency)
    "-t", "0", // run indefinitely
    "-o", "-", // write to stdout
];

/// Delay before restarting the capture subprocess after it exits or errors.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Read buffer size when draining the subprocess stdout.
const READ_CHUNK: usize = 32 * 1024;

/// Hard cap on the reassembly buffer; guards against unbounded growth if the
/// stream never yields a start-of-image marker.
const MAX_BUFFER: usize = 4 * 1024 * 1024;

/// Camera orientation. Rotation 0/180 and the flips are applied by `rpicam-vid`
/// at the sensor/ISP level (free). 90/270 (clockwise) can't be done by
/// libcamera, so those are a per-frame lossless JPEG rotate in software
/// ([`crate::rotate`], ~94 ms/frame on a Zero 2 W). Flips apply before the
/// rotate. The recorder sees the rotated frames too.
#[derive(Clone, Copy, Debug)]
pub struct Orientation {
    pub rotation: u16, // 0, 90, 180 or 270 (clockwise)
    pub hflip: bool,
    pub vflip: bool,
}

/// Parse a rotation value: 90/180/270 or anything else => 0.
pub fn parse_rotation(v: &str) -> u16 {
    match v.trim() {
        "90" => 90,
        "180" => 180,
        "270" => 270,
        _ => 0,
    }
}

impl Orientation {
    /// Initial orientation from the environment (set via record.conf / the
    /// systemd unit): `HQCAM_ROTATION` (0|90|180|270), `HQCAM_HFLIP`,
    /// `HQCAM_VFLIP` (1/true/yes/on = enabled). Absent or unparseable => 0.
    pub fn from_env() -> Self {
        Self {
            rotation: parse_rotation(&std::env::var("HQCAM_ROTATION").unwrap_or_default()),
            hflip: env_flag("HQCAM_HFLIP"),
            vflip: env_flag("HQCAM_VFLIP"),
        }
    }

    /// True when the rotation needs the software (90/270) path.
    fn software_rotate(&self) -> bool {
        matches!(self.rotation, 90 | 270)
    }
}

/// Capture size / rate / JPEG quality, read once at startup from the environment
/// (record.conf): `HQCAM_WIDTH`, `HQCAM_HEIGHT`, `HQCAM_FRAMERATE`, `HQCAM_QUALITY`.
/// Absent => default; out-of-range or unparseable => default + a warning.
///
/// Keep a 4:3 size (the IMX219's native aspect) to use the full field of view —
/// a 16:9 size like 1280x720 crops the top/bottom and looks "zoomed in".
///
/// rpicam-vid's MJPEG encode is software (libjpeg), so CPU scales with
/// pixels × fps. Measured with an IMX219 (2026-11-21):
/// - Zero W (1 core): 1296x972@15 q80 pinned rpicam-vid at ~92% CPU yet only
///   managed ~4.5 fps (one board dropped off WiFi). 640x480@10 q70 → ~7.4 fps at
///   ~67%; 800x600@10 ~86%; 1024x768@8 ~93%. Use 640x480@10 q70 on a Zero W.
/// - Zero 2 W (4 cores): 1296x972@15 q80 → full 15 fps at ~76% of one core
///   (~20% total), 44°C, no throttling. The defaults below target this board.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub quality: u32,
}

impl Default for Capture {
    fn default() -> Self {
        Self { width: 1296, height: 972, framerate: 15, quality: 80 }
    }
}

impl Capture {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Testable core of [`from_env`](Self::from_env): `get` maps a key to its value.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        // IMX219 max is 3280x2464; rpicam wants even dimensions.
        let num = |key: &str, default: u32, min: u32, max: u32, even: bool| -> u32 {
            let Some(raw) = get(key) else { return default };
            match raw.trim().parse::<u32>() {
                Ok(v) if (min..=max).contains(&v) && (!even || v % 2 == 0) => v,
                _ => {
                    log::warn!(
                        "{key}={raw:?} is invalid (want {min}..={max}{}); using {default}",
                        if even { ", even" } else { "" }
                    );
                    default
                }
            }
        };
        Self {
            width: num("HQCAM_WIDTH", d.width, 64, 3280, true),
            height: num("HQCAM_HEIGHT", d.height, 64, 2464, true),
            framerate: num("HQCAM_FRAMERATE", d.framerate, 1, 60, false),
            quality: num("HQCAM_QUALITY", d.quality, 1, 100, false),
        }
    }
}

/// `HQCAM_TUNING_FILE`: a libcamera tuning JSON passed as `--tuning-file`. Used
/// for NoIR modules, whose default profile renders everything pink/magenta (no IR
/// filter) — e.g. `/usr/share/libcamera/ipa/rpi/vc4/imx219_noir.json`. A missing
/// file is ignored with a warning, so a typo can't crash-loop rpicam-vid.
fn tuning_file_from_env() -> Option<String> {
    let path = std::env::var("HQCAM_TUNING_FILE").ok()?.trim().to_string();
    if path.is_empty() {
        return None;
    }
    if std::path::Path::new(&path).is_file() {
        Some(path)
    } else {
        log::warn!("HQCAM_TUNING_FILE={path:?} does not exist; using the default tuning");
        None
    }
}

/// Highest accepted `HQCAM_LENS_POSITION` (dioptres). The Camera Module 3's
/// closest focus is ~10 cm (= 10 dioptres); this only guards against typos.
const MAX_LENS_POSITION: f32 = 32.0;

/// `HQCAM_LENS_POSITION`: fixed manual focus for autofocus modules (Camera
/// Module 3 / IMX708), so the lens doesn't hunt on vehicle vibration. The value
/// is in dioptres (1 / distance in metres): `0` = infinity (the dashcam
/// choice), `0.5` = 2 m, `default` = the lens's hyperfocal position. Unset =>
/// rpicam-vid's own autofocus behaviour. Invalid => ignored with a warning.
/// Leave unset on fixed-focus modules (IMX219 / v2) — they have no lens control.
fn lens_position_from_env() -> Option<String> {
    let raw = std::env::var("HQCAM_LENS_POSITION").ok()?;
    let parsed = parse_lens_position(&raw);
    if parsed.is_none() && !raw.trim().is_empty() {
        log::warn!(
            "HQCAM_LENS_POSITION={raw:?} is invalid (want 0..={MAX_LENS_POSITION} or \"default\"); \
             leaving focus to rpicam-vid"
        );
    }
    parsed
}

/// Validate a lens position: `default` or a number in `0..=MAX_LENS_POSITION`.
/// Returns the value to pass to `--lens-position`, or `None` if empty/invalid.
pub fn parse_lens_position(v: &str) -> Option<String> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("default") {
        return Some("default".into());
    }
    match v.parse::<f32>() {
        Ok(p) if (0.0..=MAX_LENS_POSITION).contains(&p) => Some(v.into()),
        _ => None,
    }
}

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Live capture control shared between the HTTP layer and the capture thread.
///
/// Holds the current [`Orientation`] and a handle to the running `rpicam-vid`
/// child. `rpicam-vid` can't be re-oriented in place, so [`set`](Self::set)
/// updates the orientation and kills the child; the supervisor loop in [`start`]
/// then respawns it with the new flags (a ~1s stream blip).
pub struct CamControl {
    capture: Capture,
    /// Optional libcamera tuning file (`--tuning-file`), e.g. the NoIR profile.
    tuning_file: Option<String>,
    /// Optional fixed focus (`--lens-position`) for autofocus modules. Live:
    /// [`set_focus`](Self::set_focus) changes it and restarts the capture.
    lens_position: Mutex<Option<String>>,
    orientation: Mutex<Orientation>,
    child: Mutex<Option<Child>>,
    /// libturbojpeg, loaded on first use of a 90/270 rotation.
    rotator: OnceLock<Option<Rotator>>,
}

impl CamControl {
    pub fn from_env() -> Arc<Self> {
        let capture = Capture::from_env();
        let tuning_file = tuning_file_from_env();
        let lens_position = lens_position_from_env();
        log::info!(
            "capture: {}x{} @ {} fps, JPEG quality {}, tuning {}, focus {}",
            capture.width, capture.height, capture.framerate, capture.quality,
            tuning_file.as_deref().unwrap_or("default"),
            lens_position.as_deref().map_or("auto".into(), |p| format!("fixed {p}"))
        );
        Arc::new(Self {
            capture,
            tuning_file,
            lens_position: Mutex::new(lens_position),
            orientation: Mutex::new(Orientation::from_env()),
            child: Mutex::new(None),
            rotator: OnceLock::new(),
        })
    }

    /// The software rotator, loaded lazily (only when 90/270 is first used).
    fn rotator(&self) -> Option<&Rotator> {
        self.rotator.get_or_init(Rotator::load).as_ref()
    }

    /// The current orientation.
    pub fn get(&self) -> Orientation {
        *self.orientation.lock().unwrap()
    }

    /// Capture size / rate / quality (fixed for the life of the process).
    pub fn capture(&self) -> Capture {
        self.capture
    }

    /// The libcamera tuning file in use, if any.
    pub fn tuning_file(&self) -> Option<&str> {
        self.tuning_file.as_deref()
    }

    /// The fixed lens position, or `None` for the camera's own autofocus.
    pub fn focus(&self) -> Option<String> {
        self.lens_position.lock().unwrap().clone()
    }

    /// Fix focus at a lens position (validated by the caller with
    /// [`parse_lens_position`]) or `None` for autofocus, then restart the
    /// capture subprocess so it takes effect (~1s stream blip).
    pub fn set_focus(&self, lens_position: Option<String>) {
        *self.lens_position.lock().unwrap() = lens_position;
        self.restart();
    }

    /// Kill the running `rpicam-vid`; the supervisor respawns it with the
    /// current settings.
    fn restart(&self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    /// Apply a new orientation and force the capture subprocess to restart so it
    /// takes effect. No-op restart if no child is running yet (the next spawn
    /// picks up the new value anyway).
    pub fn set(&self, o: Orientation) {
        *self.orientation.lock().unwrap() = o;
        self.restart();
    }
}

/// Build the full `rpicam-vid` argument list for a capture config, optional
/// tuning file, optional fixed lens position and orientation.
fn rpicam_args(
    c: &Capture,
    tuning_file: Option<&str>,
    lens_position: Option<&str>,
    o: &Orientation,
) -> Vec<String> {
    let mut args: Vec<String> = RPICAM_BASE_ARGS.iter().map(|s| (*s).to_string()).collect();
    if let Some(t) = tuning_file {
        args.push("--tuning-file".into());
        args.push(t.into());
    }
    if let Some(p) = lens_position {
        args.extend(["--autofocus-mode".into(), "manual".into()]);
        args.extend(["--lens-position".into(), p.into()]);
    }
    for (flag, v) in [
        ("--width", c.width),
        ("--height", c.height),
        ("--framerate", c.framerate),
        ("--quality", c.quality),
    ] {
        args.push(flag.into());
        args.push(v.to_string());
    }
    args.push("--rotation".into());
    // 90/270 are done in software after capture (see `crate::rotate`).
    args.push(if o.rotation == 180 { "180".into() } else { "0".into() });
    if o.hflip {
        args.push("--hflip".into());
    }
    if o.vflip {
        args.push("--vflip".into());
    }
    args
}

/// A single-slot broadcast of the most recent JPEG frame.
///
/// Mirrors the Python `StreamingOutput` + `threading.Condition`: one producer
/// publishes frames; any number of consumers block until a frame newer than the
/// last one they observed is available.
pub struct FrameBus {
    inner: Mutex<FrameState>,
    cond: Condvar,
}

struct FrameState {
    frame: Option<Arc<Vec<u8>>>,
    seq: u64,
    /// When the current frame was published (for the status page's "camera
    /// alive?" check).
    at: Option<Instant>,
}

impl FrameBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FrameState { frame: None, seq: 0, at: None }),
            cond: Condvar::new(),
        })
    }

    fn publish(&self, frame: Vec<u8>) {
        let mut state = self.inner.lock().unwrap();
        state.frame = Some(Arc::new(frame));
        state.seq = state.seq.wrapping_add(1);
        state.at = Some(Instant::now());
        self.cond.notify_all();
    }

    /// Wait up to `timeout` for the next frame (one newer than the current), so
    /// a snapshot is never a stale frame from a stalled capture. `None` if no
    /// frame arrives in time.
    pub fn next_frame(&self, timeout: Duration) -> Option<Arc<Vec<u8>>> {
        let state = self.inner.lock().unwrap();
        let seq = state.seq;
        let (state, _) = self.cond.wait_timeout_while(state, timeout, |s| s.seq == seq).unwrap();
        if state.seq == seq {
            None
        } else {
            state.frame.clone()
        }
    }

    /// How long ago the latest frame arrived; `None` before the first frame.
    pub fn last_frame_age(&self) -> Option<Duration> {
        self.inner.lock().unwrap().at.map(|t| t.elapsed())
    }

    /// Block until a frame newer than `last_seq` exists, returning it and its
    /// sequence number. Pass `0` on the first call.
    pub fn wait_for(&self, last_seq: u64) -> (Arc<Vec<u8>>, u64) {
        let mut state = self.inner.lock().unwrap();
        while state.frame.is_none() || state.seq == last_seq {
            state = self.cond.wait(state).unwrap();
        }
        (state.frame.clone().unwrap(), state.seq)
    }
}

/// Spawn the supervised capture thread. `control` supplies the live orientation
/// and lets the HTTP layer trigger a re-orient restart.
pub fn start(bus: Arc<FrameBus>, control: Arc<CamControl>) {
    thread::spawn(move || loop {
        match run_capture(&bus, &control) {
            Ok(()) => log::warn!("rpicam-vid exited; restarting"),
            Err(e) => log::error!("camera capture failed: {e}; restarting"),
        }
        thread::sleep(RESTART_DELAY);
    });
}

fn run_capture(bus: &FrameBus, control: &CamControl) -> std::io::Result<()> {
    let orientation = control.get();
    let lens_position = control.focus();
    let args = rpicam_args(
        &control.capture,
        control.tuning_file.as_deref(),
        lens_position.as_deref(),
        &orientation,
    );
    // Orientation changes restart this function, so it's fixed per child.
    let rotate = if orientation.software_rotate() {
        control.rotator().map(|r| (r, orientation.rotation))
    } else {
        None
    };
    let mut child = Command::new("rpicam-vid")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    // Publish the child so control.set() can kill it to force a re-orient restart.
    *control.child.lock().unwrap() = Some(child);

    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut chunk = [0u8; READ_CHUNK];

    let result = (|| loop {
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // subprocess closed its stdout
        }
        buf.extend_from_slice(&chunk[..n]);
        drain_frames(&mut buf, bus, rotate);
    })();

    // Always reap the child so a restart starts from a clean slate. Take it back
    // from the shared slot (a control.set() may have already killed it).
    if let Some(mut child) = control.child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Pull every complete `SOI..EOI` JPEG out of `buf` and publish it, rotating it
/// first when `rotate` is set (a failed rotate publishes the original frame).
///
/// MJPEG frames from the hardware encoder are baseline JPEGs with no embedded
/// thumbnail, so a plain marker scan (FFD8 start, FFD9 end) is unambiguous.
fn drain_frames(buf: &mut Vec<u8>, bus: &FrameBus, rotate: Option<(&Rotator, u16)>) {
    loop {
        let Some(soi) = find_marker(buf, 0, 0xD8) else {
            // No start-of-image yet; discard garbage so we don't grow forever.
            if buf.len() > MAX_BUFFER {
                buf.clear();
            }
            return;
        };
        if soi > 0 {
            buf.drain(..soi); // drop anything before the start marker
        }
        let Some(eoi) = find_marker(buf, 2, 0xD9) else {
            return; // frame not fully buffered yet
        };
        let end = eoi + 2;
        let frame = match rotate {
            Some((r, deg)) => r.rotate(&buf[..end], deg).unwrap_or_else(|| buf[..end].to_vec()),
            None => buf[..end].to_vec(),
        };
        buf.drain(..end);
        bus.publish(frame);
    }
}

/// Find a JPEG marker (`0xFF <marker>`) at or after `from`.
fn find_marker(data: &[u8], from: usize, marker: u8) -> Option<usize> {
    let mut i = from;
    while i + 1 < data.len() {
        if data[i] == 0xFF && data[i + 1] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A [`Read`] adapter that turns the [`FrameBus`] into an endless
/// `multipart/x-mixed-replace` body. Each `read` yields bytes from the current
/// multipart part, blocking for the next frame when that part is exhausted.
pub struct MjpegStream {
    bus: Arc<FrameBus>,
    last_seq: u64,
    pending: Vec<u8>,
    pos: usize,
}

impl MjpegStream {
    pub fn new(bus: Arc<FrameBus>) -> Self {
        Self { bus, last_seq: 0, pending: Vec::new(), pos: 0 }
    }
}

impl Read for MjpegStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.pending.len() {
            let (frame, seq) = self.bus.wait_for(self.last_seq);
            self.last_seq = seq;

            let mut part = Vec::with_capacity(frame.len() + 96);
            part.extend_from_slice(b"--FRAME\r\n");
            part.extend_from_slice(b"Content-Type: image/jpeg\r\n");
            part.extend_from_slice(format!("Content-Length: {}\r\n\r\n", frame.len()).as_bytes());
            part.extend_from_slice(&frame);
            part.extend_from_slice(b"\r\n");

            self.pending = part;
            self.pos = 0;
        }

        let mut src = &self.pending[self.pos..];
        let n = src.read(out)?;
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn capture_from(pairs: &[(&str, &str)]) -> Capture {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Capture::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn capture_defaults_when_unset() {
        assert_eq!(capture_from(&[]), Capture::default());
    }

    #[test]
    fn capture_reads_valid_values() {
        let c = capture_from(&[
            ("HQCAM_WIDTH", "1640"),
            ("HQCAM_HEIGHT", " 1232 "),
            ("HQCAM_FRAMERATE", "10"),
            ("HQCAM_QUALITY", "85"),
        ]);
        assert_eq!(c, Capture { width: 1640, height: 1232, framerate: 10, quality: 85 });
    }

    #[test]
    fn capture_rejects_bad_values_per_field() {
        let d = Capture::default();
        let c = capture_from(&[
            ("HQCAM_WIDTH", "641"),      // odd
            ("HQCAM_HEIGHT", "9999"),    // above sensor max
            ("HQCAM_FRAMERATE", "0"),    // below min
            ("HQCAM_QUALITY", "high"),   // not a number
        ]);
        assert_eq!(c, d);
    }

    #[test]
    fn rpicam_args_carry_capture_and_orientation() {
        let c = Capture { width: 1640, height: 1232, framerate: 10, quality: 85 };
        let o = Orientation { rotation: 180, hflip: false, vflip: true };
        let a = rpicam_args(&c, None, None, &o).join(" ");
        for want in ["--width 1640", "--height 1232", "--framerate 10", "--quality 85",
                     "--rotation 180", "--vflip", "--codec mjpeg", "-o -"] {
            assert!(a.contains(want), "missing {want:?} in {a}");
        }
        assert!(!a.contains("--hflip"));
        assert!(!a.contains("--tuning-file"));
        assert!(!a.contains("--lens-position"), "no fixed focus unless set: {a}");
    }

    #[test]
    fn lens_position_parses_numbers_and_default() {
        assert_eq!(parse_lens_position(" 0 ").as_deref(), Some("0"));
        assert_eq!(parse_lens_position("0.5").as_deref(), Some("0.5"));
        assert_eq!(parse_lens_position("Default").as_deref(), Some("default"));
        for bad in ["", "-1", "33", "far", "NaN", "inf"] {
            assert_eq!(parse_lens_position(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rpicam_args_fix_focus_when_lens_position_set() {
        let o = Orientation { rotation: 0, hflip: false, vflip: false };
        let a = rpicam_args(&Capture::default(), None, Some("0"), &o).join(" ");
        assert!(a.contains("--autofocus-mode manual --lens-position 0"), "{a}");
    }

    #[test]
    fn rpicam_args_include_tuning_file_when_set() {
        let t = "/usr/share/libcamera/ipa/rpi/vc4/imx219_noir.json";
        let a = rpicam_args(&Capture::default(), Some(t), None, &Orientation { rotation: 0, hflip: false, vflip: false });
        assert!(a.windows(2).any(|w| w[0] == "--tuning-file" && w[1] == t), "{a:?}");
    }

    #[test]
    fn quarter_turns_are_software_and_leave_rpicam_at_zero() {
        assert_eq!(parse_rotation(" 270 "), 270);
        assert_eq!(parse_rotation("45"), 0);
        let o = Orientation { rotation: 270, hflip: false, vflip: false };
        assert!(o.software_rotate());
        let a = rpicam_args(&Capture::default(), None, None, &o).join(" ");
        assert!(a.contains("--rotation 0"), "{a}");
    }
}
