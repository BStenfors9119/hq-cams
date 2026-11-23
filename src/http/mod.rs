//! The HTTP layer: the live stream, the web UI and its JSON API.
//!
//! LAN only, no login (yet) — anyone who can reach port 6877 can view and
//! control the camera.
//!
//! ```text
//! GET    /                               -> 301 /index.html
//! GET    /index.html /app.js /app.css    the web UI
//! GET    /stream.mjpg                    live MJPEG (what Motion Eye pulls)
//! GET    /snapshot.jpg                   the next live frame, not saved
//! GET    /api/status                     health + camera + recorder + snapshots
//! GET    /api/snapshots                  saved pictures, newest first
//! POST   /api/snapshots                  Take a Pic -> 201 + the new picture
//!                                        (409 when no USB/network drive is mounted)
//! POST   /api/snapshots/download         Take a Pic, download-only: the JPEG comes
//!                                        back as an attachment, never saved on the camera
//! DELETE /api/snapshots/{date}/{name}
//! GET    /snapshots/{date}/{name}        a saved picture (?download=1 to save it)
//! GET    /api/record                     recorder status
//! POST   /api/record      {"on":bool}    manual Record switch (needs the USB drive)
//! GET    /api/orientation
//! POST   /api/orientation {"rotation":0|90|180|270,"hflip":bool,"vflip":bool}  (any subset)
//! GET    /api/focus
//! POST   /api/focus       {"lensPosition": null | number | "default"}
//! GET|POST /orientation?rotation=&hflip=&vflip=   legacy query-string form (not saved)
//! ```
//!
//! Orientation and focus changes made through `/api/…` are saved to
//! `record.conf`, so they survive a restart; each restarts the capture (~1s blip).

use std::fs::File;
use std::io::{Cursor, Read};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::cam::{self, CamControl, Capture, FrameBus, MjpegStream, Orientation};
use crate::conf;
use crate::recorder::{self, Recorder, RecorderStatus};
use crate::snapshot::{Snapshot, SnapshotError, Snapshots, Storage};
use crate::status::{self, Health};

const INDEX_HTML: &str = include_str!("ui/index.html");
const APP_JS: &str = include_str!("ui/app.js");
const APP_CSS: &str = include_str!("ui/app.css");

/// Largest JSON request body accepted (they're all tiny).
const MAX_BODY: u64 = 4 * 1024;

/// Everything a request handler can reach.
#[derive(Clone)]
pub struct App {
    pub bus: Arc<FrameBus>,
    pub cam: Arc<CamControl>,
    pub recorder: Arc<Recorder>,
    pub snapshots: Arc<Snapshots>,
}

type Reply = Response<Cursor<Vec<u8>>>;

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}

fn json<T: Serialize>(status: u16, body: &T) -> Reply {
    let mut text = serde_json::to_string(body).expect("serializable");
    text.push('\n');
    Response::from_string(text)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
}

fn error(status: u16, message: impl std::fmt::Display) -> Reply {
    #[derive(Serialize)]
    struct Error {
        error: String,
    }
    json(status, &Error { error: message.to_string() })
}

fn text(body: &'static str, content_type: &str) -> Reply {
    Response::from_string(body).with_header(header("Content-Type", content_type))
}

fn jpeg(bytes: Vec<u8>) -> Reply {
    Response::from_data(bytes)
        .with_header(header("Content-Type", "image/jpeg"))
        .with_header(header("Cache-Control", "no-store"))
}

fn snapshot_error(e: SnapshotError) -> Reply {
    match e {
        SnapshotError::NoFrame => error(503, e),
        SnapshotError::NoStorage => error(409, e),
        SnapshotError::Io(_) => error(500, e),
    }
}

/// A download-only picture: straight from memory to the requesting device.
/// Named `hqcam-YYYY-MM-DD-HHMMSS.jpg` (UTC) so repeated downloads don't collide.
fn download_only(bytes: Vec<u8>) -> Reply {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_secs());
    let (y, m, d) = recorder::ymd(secs);
    let t = secs % 86_400;
    let name = format!("hqcam-{y:04}-{m:02}-{d:02}-{:02}{:02}{:02}.jpg", t / 3600, t / 60 % 60, t % 60);
    jpeg(bytes).with_header(header("Content-Disposition", &format!("attachment; filename=\"{name}\"")))
}

fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T, Reply> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|e| error(400, format!("couldn't read the request: {e}")))?;
    serde_json::from_str(&body).map_err(|e| error(400, format!("bad JSON: {e}")))
}

pub fn handle(mut request: Request, app: App) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = request.method().clone();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    // The two long-lived / file responses have their own body types.
    match (&method, segments.as_slice()) {
        (Method::Get, ["stream.mjpg"]) => return stream(request, app.bus),
        (Method::Get, ["snapshots", date, name]) => {
            return serve_snapshot(request, &app.snapshots, date, name, query.contains("download=1"))
        }
        _ => {}
    }

    let reply = match (&method, segments.as_slice()) {
        (Method::Get, [""]) => Response::from_string("")
            .with_status_code(StatusCode(301))
            .with_header(header("Location", "/index.html")),
        (Method::Get, ["index.html"]) => text(INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Get, ["app.js"]) => text(APP_JS, "text/javascript; charset=utf-8"),
        (Method::Get, ["app.css"]) => text(APP_CSS, "text/css; charset=utf-8"),
        (Method::Get, ["snapshot.jpg"]) => match app.bus.next_frame(std::time::Duration::from_secs(3)) {
            Some(frame) => jpeg(frame.to_vec()),
            None => error(503, SnapshotError::NoFrame),
        },

        (Method::Get, ["api", "status"]) => json(200, &full_status(&app)),

        (Method::Get, ["api", "snapshots"]) => json(200, &app.snapshots.list()),
        (Method::Post, ["api", "snapshots"]) => match app.snapshots.take(&app.bus) {
            Ok((snap, _)) => json(201, &snap),
            Err(e) => snapshot_error(e),
        },
        (Method::Post, ["api", "snapshots", "download"]) => match app.snapshots.grab(&app.bus) {
            Ok(frame) => download_only(frame.to_vec()),
            Err(e) => snapshot_error(e),
        },
        (Method::Delete, ["api", "snapshots", date, name]) => match app.snapshots.delete(date, name) {
            Ok(true) => Response::from_string("").with_status_code(StatusCode(204)),
            Ok(false) => error(404, "no such picture"),
            Err(e) => error(500, format!("couldn't delete it: {e}")),
        },

        (Method::Get, ["api", "record"]) => json(200, &app.recorder.status()),
        (Method::Post, ["api", "record"]) => post_record(&mut request, &app.recorder),

        (Method::Get, ["api", "orientation"]) => json(200, &OrientationJson::from(app.cam.get())),
        (Method::Post, ["api", "orientation"]) => post_orientation(&mut request, &app.cam),

        (Method::Get, ["api", "focus"]) => json(200, &FocusJson::from(app.cam.focus())),
        (Method::Post, ["api", "focus"]) => post_focus(&mut request, &app.cam),

        (Method::Get | Method::Post, ["orientation"]) => legacy_orientation(&app.cam, query),

        (_, ["api", ..]) | (_, ["stream.mjpg" | "snapshot.jpg" | "index.html"]) => {
            error(405, "method not allowed")
        }
        _ => error(404, "not found"),
    };
    if let Err(e) = request.respond(reply) {
        log::debug!("response failed: {e}");
    }
}

fn stream(request: Request, bus: Arc<FrameBus>) {
    let response = Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", "multipart/x-mixed-replace; boundary=FRAME"),
            header("Cache-Control", "no-cache, private"),
            header("Pragma", "no-cache"),
            header("Age", "0"),
        ],
        MjpegStream::new(bus),
        None, // no Content-Length -> chunked; runs until the client disconnects
        None,
    );
    // A client dropping mid-stream is the normal way an MJPEG view ends.
    if let Err(e) = request.respond(response) {
        log::debug!("stream ended: {e}");
    }
}

fn serve_snapshot(request: Request, snapshots: &Snapshots, date: &str, name: &str, download: bool) {
    let result = match snapshots.path_of(date, name).and_then(|p| File::open(p).ok()) {
        Some(file) => {
            let mut response = Response::from_file(file)
                .with_header(header("Content-Type", "image/jpeg"))
                .with_header(header("Cache-Control", "no-cache"));
            if download {
                let stem = name.trim_end_matches(".jpg");
                response = response.with_header(header(
                    "Content-Disposition",
                    &format!("attachment; filename=\"hqcam-{date}-{stem}.jpg\""),
                ));
            }
            request.respond(response)
        }
        None => request.respond(error(404, "no such picture")),
    };
    if let Err(e) = result {
        log::debug!("response failed: {e}");
    }
}

// ---- status ----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FullStatus {
    health: Health,
    camera: CameraStatus,
    recorder: RecorderStatus,
    snapshots: SnapshotsStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CameraStatus {
    width: u32,
    height: u32,
    framerate: u32,
    quality: u32,
    tuning_file: Option<String>,
    orientation: OrientationJson,
    focus: FocusJson,
    /// Milliseconds since the last frame; `null` before the first. Over a
    /// second or two means the camera has stalled.
    last_frame_ms: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotsStatus {
    /// Where pictures are saved; `null` = no drive, download-only.
    storage: Option<Storage>,
    count: usize,
    free_mb: Option<u64>,
    latest: Option<Snapshot>,
}

fn full_status(app: &App) -> FullStatus {
    let Capture { width, height, framerate, quality } = app.cam.capture();
    let storage = app.snapshots.storage();
    let mut list = app.snapshots.list();
    let count = list.len();
    FullStatus {
        health: status::health(),
        camera: CameraStatus {
            width,
            height,
            framerate,
            quality,
            tuning_file: app.cam.tuning_file().map(str::to_string),
            orientation: app.cam.get().into(),
            focus: app.cam.focus().into(),
            last_frame_ms: app.bus.last_frame_age().map(|d| d.as_millis() as u64),
        },
        recorder: app.recorder.status(),
        snapshots: SnapshotsStatus {
            free_mb: storage.as_ref().and_then(|s| recorder::free_mb(&s.mount)),
            storage,
            count,
            latest: if list.is_empty() { None } else { Some(list.swap_remove(0)) },
        },
    }
}

// ---- record ----------------------------------------------------------------

fn post_record(request: &mut Request, recorder: &Recorder) -> Reply {
    #[derive(Deserialize)]
    struct Body {
        on: bool,
    }
    let body: Body = match read_json(request) {
        Ok(b) => b,
        Err(reply) => return reply,
    };
    match recorder.set_manual(body.on) {
        Ok(()) => json(200, &recorder.status()),
        Err(e) => error(409, e),
    }
}

// ---- orientation -----------------------------------------------------------

#[derive(Serialize)]
struct OrientationJson {
    rotation: u16,
    hflip: bool,
    vflip: bool,
}

impl From<Orientation> for OrientationJson {
    fn from(o: Orientation) -> Self {
        Self { rotation: o.rotation, hflip: o.hflip, vflip: o.vflip }
    }
}

fn post_orientation(request: &mut Request, cam: &CamControl) -> Reply {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        rotation: Option<u16>,
        hflip: Option<bool>,
        vflip: Option<bool>,
    }
    let body: Body = match read_json(request) {
        Ok(b) => b,
        Err(reply) => return reply,
    };
    let mut o = cam.get();
    if let Some(r) = body.rotation {
        if !matches!(r, 0 | 90 | 180 | 270) {
            return error(400, "rotation must be 0, 90, 180 or 270");
        }
        o.rotation = r;
    }
    o.hflip = body.hflip.unwrap_or(o.hflip);
    o.vflip = body.vflip.unwrap_or(o.vflip);
    cam.set(o);
    log::info!("orientation set via API: {o:?}");
    let flag = |b: bool| if b { "1" } else { "0" };
    let rotation = o.rotation.to_string();
    save(&[("HQCAM_ROTATION", &rotation), ("HQCAM_HFLIP", flag(o.hflip)), ("HQCAM_VFLIP", flag(o.vflip))]);
    json(200, &OrientationJson::from(o))
}

/// `GET|POST /orientation?rotation=&hflip=&vflip=` — the original query-string
/// API, kept for existing scripts. In-memory only (not saved).
fn legacy_orientation(cam: &CamControl, query: &str) -> Reply {
    let truthy = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    let mut o = cam.get();
    let mut changed = false;
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "rotation" => o.rotation = cam::parse_rotation(v),
            "hflip" => o.hflip = truthy(v),
            "vflip" => o.vflip = truthy(v),
            _ => continue,
        }
        changed = true;
    }
    if changed {
        cam.set(o);
        log::info!("orientation set via HTTP: {o:?}");
    }
    json(200, &OrientationJson::from(o))
}

// ---- focus -----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FocusJson {
    /// `auto`, or `fixed` at `lensPosition`.
    mode: &'static str,
    lens_position: Option<String>,
}

impl From<Option<String>> for FocusJson {
    fn from(p: Option<String>) -> Self {
        Self { mode: if p.is_some() { "fixed" } else { "auto" }, lens_position: p }
    }
}

fn post_focus(request: &mut Request, cam: &CamControl) -> Reply {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        lens_position: Value,
    }
    let body: Body = match read_json(request) {
        Ok(b) => b,
        Err(reply) => return reply,
    };
    let lens = match &body.lens_position {
        Value::Null => None,
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => return error(400, "lensPosition must be null (autofocus), a number, or \"default\""),
    };
    let lens = match lens {
        None => None,
        Some(raw) => match cam::parse_lens_position(&raw) {
            Some(p) => Some(p),
            None => return error(400, "lensPosition must be 0–32 (dioptres: 0 = infinity) or \"default\""),
        },
    };
    cam.set_focus(lens.clone());
    log::info!("focus set via API: {}", lens.as_deref().unwrap_or("auto"));
    save(&[("HQCAM_LENS_POSITION", lens.as_deref().unwrap_or(""))]);
    json(200, &FocusJson::from(lens))
}

/// Save settings to record.conf. A failure is logged, not returned: the change
/// is already live, it just won't survive a restart.
fn save(pairs: &[(&str, &str)]) {
    let path = conf::path();
    if let Err(e) = conf::set(&path, pairs) {
        log::error!("couldn't save settings to {}: {e} (applied until restart)", path.display());
    }
}
