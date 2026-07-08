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
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// `rpicam-vid` arguments. Tune resolution / framerate / quality here.
///
/// Resolution is kept at the sensor's native 4:3 aspect (IMX219) so the full
/// field of view is used. A 16:9 size like 1280x720 crops the top/bottom of the
/// 4:3 sensor, which looks "zoomed in". Orientation: the camera is mounted the
/// normal way up, so no rotation is applied (matching the legacy farmview
/// server). If a future mount is inverted, add `--rotation 180`; for a mirror
/// use `--hflip`/`--vflip`. (rpicam-vid `--rotation` only accepts 0 or 180.)
const RPICAM_ARGS: &[&str] = &[
    "--codec", "mjpeg",
    "--width", "1296",
    "--height", "972",
    "--framerate", "15",
    "--quality", "80",
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
}

impl FrameBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FrameState { frame: None, seq: 0 }),
            cond: Condvar::new(),
        })
    }

    fn publish(&self, frame: Vec<u8>) {
        let mut state = self.inner.lock().unwrap();
        state.frame = Some(Arc::new(frame));
        state.seq = state.seq.wrapping_add(1);
        self.cond.notify_all();
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

/// Spawn the supervised capture thread.
pub fn start(bus: Arc<FrameBus>) {
    thread::spawn(move || loop {
        match run_capture(&bus) {
            Ok(()) => log::warn!("rpicam-vid exited; restarting"),
            Err(e) => log::error!("camera capture failed: {e}; restarting"),
        }
        thread::sleep(RESTART_DELAY);
    });
}

fn run_capture(bus: &FrameBus) -> std::io::Result<()> {
    let mut child = Command::new("rpicam-vid")
        .args(RPICAM_ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut chunk = [0u8; READ_CHUNK];

    let result = (|| loop {
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // subprocess closed its stdout
        }
        buf.extend_from_slice(&chunk[..n]);
        drain_frames(&mut buf, bus);
    })();

    // Always reap the child so a restart starts from a clean slate.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Pull every complete `SOI..EOI` JPEG out of `buf` and publish it.
///
/// MJPEG frames from the hardware encoder are baseline JPEGs with no embedded
/// thumbnail, so a plain marker scan (FFD8 start, FFD9 end) is unambiguous.
fn drain_frames(buf: &mut Vec<u8>, bus: &FrameBus) {
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
        let frame = buf[..end].to_vec();
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
