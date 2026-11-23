//! The link to Barbie's Lab (barbies-lab's `cameras` module): lets the camera
//! be watched and driven from www.barbieslab.com without anyone reaching into
//! the home network.
//!
//! **The camera always calls out; nothing calls in.** `sudo hqcam connect`
//! signs in once (the person's own Barbie's Lab login, used once and never
//! stored) and saves the token it gets to `uplink.conf` (root-only). Then this
//! module's thread:
//!
//! - **long-polls** `POST /api/cameras/feed/poll` with the camera's status (the
//!   same JSON as `/api/status`); the server holds it ≤25 s and answers as soon
//!   as there is something to do;
//! - **Take a Pic** (`snapshot`): sends the next live frame to
//!   `/feed/pictures?command=`. Held in memory only — Barbie's Lab is where it
//!   is kept, so nothing is written to the SD card (see `snapshot/`);
//! - **settings**: rotation / mirror / focus, applied through `settings/` exactly
//!   like the local web page (so they're saved to record.conf too);
//! - **live view**: while someone watches, a frame about once a second to
//!   `/feed/frame`, until the server says nobody is.
//!
//! HTTP goes through `curl` (a deb dependency) rather than a TLS crate: it keeps
//! the binary small and the cross-compile pure Rust, like `rpicam-vid`, `nmcli`
//! and `rsync` elsewhere. The token reaches curl through a root-only config file
//! (`-K`), never the command line, so it doesn't show up in `ps`.
//!
//! `uplink.conf` is re-read on every round, so `hqcam connect` / `disconnect`
//! take effect within seconds without restarting the service.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{self, App};
use crate::settings::{self, OrientationChange};

/// Where `hqcam connect` saves the link. `HQCAM_UPLINK_CONF` overrides.
const DEFAULT_CONF: &str = "/usr/local/etc/hqcam/uplink.conf";
/// Runtime dir for curl's config (holds the token). `HQCAM_RUN_DIR` overrides.
const DEFAULT_RUN_DIR: &str = "/run/hqcam";
/// Where `hqcam connect` signs in when no address is given.
pub const DEFAULT_URL: &str = "https://www.barbieslab.com";

const KEY_URL: &str = "HQCAM_UPLINK_URL";
const KEY_TOKEN: &str = "HQCAM_UPLINK_TOKEN";
const KEY_NAME: &str = "HQCAM_UPLINK_NAME";
const KEY_CAMERA_ID: &str = "HQCAM_UPLINK_CAMERA_ID";

const PATH_CONNECT: &str = "/api/cameras/connect";
const PATH_POLL: &str = "/api/cameras/feed/poll";
const PATH_FRAME: &str = "/api/cameras/feed/frame";
const PATH_PICTURES: &str = "/api/cameras/feed/pictures";
const PATH_COMMANDS: &str = "/api/cameras/feed/commands";

const JSON: &str = "application/json";
const JPEG: &str = "image/jpeg";

/// The server holds a poll ≤25 s; allow for that plus a slow link.
const POLL_TIMEOUT: Duration = Duration::from_secs(45);
const SEND_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to look for `uplink.conf` while not connected.
const IDLE_CHECK: Duration = Duration::from_secs(10);
/// Back-off after a failed poll: doubles from MIN to MAX.
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// After the server rejects the token, wait this long before trying it again.
const REJECTED_WAIT: Duration = Duration::from_secs(120);
/// While backing off, how often to look for `hqcam connect` / `disconnect`.
const CONF_CHECK: Duration = Duration::from_secs(2);
/// Live view: one frame per this long.
const LIVE_INTERVAL: Duration = Duration::from_secs(1);
const FRAME_WAIT: Duration = Duration::from_secs(3);

fn conf_path() -> PathBuf {
    std::env::var("HQCAM_UPLINK_CONF").unwrap_or_else(|_| DEFAULT_CONF.into()).into()
}

fn run_dir() -> PathBuf {
    std::env::var("HQCAM_RUN_DIR").unwrap_or_else(|_| DEFAULT_RUN_DIR.into()).into()
}

// ── uplink.conf ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
struct Link {
    url: String,
    token: String,
    name: String,
    camera_id: Option<i64>,
}

impl Link {
    fn load(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let get = |key: &str| {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Some(Self {
            url: get(KEY_URL)?.trim_end_matches('/').to_string(),
            token: get(KEY_TOKEN)?,
            name: get(KEY_NAME).unwrap_or_default(),
            camera_id: get(KEY_CAMERA_ID).and_then(|v| v.parse().ok()),
        })
    }

    /// Root-only (0600), written to a temp file and renamed into place.
    fn save(&self, path: &Path) -> io::Result<()> {
        let text = format!(
            "# Written by `hqcam connect` — the link to Barbie's Lab. Holds a secret\n\
             # token: keep it root-only. `hqcam disconnect` removes it.\n\
             {KEY_URL}={}\n{KEY_TOKEN}={}\n{KEY_NAME}={}\n{KEY_CAMERA_ID}={}\n",
            self.url,
            self.token,
            self.name,
            self.camera_id.map(|i| i.to_string()).unwrap_or_default()
        );
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("conf.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    }
}

// ── curl ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum CallError {
    /// curl couldn't run, or the network failed (curl's own message).
    Network(String),
    /// The server answered with an error status.
    Status(u16, String),
    /// The server's answer wasn't the JSON we expected.
    BadAnswer(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "can't reach the server: {e}"),
            Self::Status(code, body) => {
                // The server's `{"error": "..."}` reads better than raw JSON.
                let detail = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| body.chars().take(200).collect());
                write!(f, "the server said {code}: {detail}")
            }
            Self::BadAnswer(e) => write!(f, "unexpected answer from the server: {e}"),
        }
    }
}

impl CallError {
    /// The token was refused (revoked on the Users screen, or the camera removed).
    fn rejected(&self) -> bool {
        matches!(self, Self::Status(401 | 403, _))
    }
}

/// One HTTP POST through curl. `auth`: a curl config file holding the
/// `Authorization` header (`-K`), so the token never appears in argv.
fn post(url: &str, auth: Option<&Path>, content_type: &str, body: &[u8], timeout: Duration) -> Result<Vec<u8>, CallError> {
    // Appended by `--write-out` after the body. No `%` in the marker: curl
    // reads `%` there as the start of a variable.
    const STATUS_MARK: &[u8] = b"\nHQCAM-HTTP-STATUS:";
    let mut cmd = Command::new("curl");
    cmd.args(["--silent", "--show-error", "--request", "POST"])
        .args(["--connect-timeout", &CONNECT_TIMEOUT.as_secs().to_string()])
        .args(["--max-time", &timeout.as_secs().to_string()])
        .args(["--header", &format!("Content-Type: {content_type}")])
        .args(["--data-binary", "@-"])
        .args(["--write-out", "\nHQCAM-HTTP-STATUS:%{http_code}"]);
    if let Some(auth) = auth {
        cmd.arg("--config").arg(auth);
    }
    let mut child = cmd
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CallError::Network(format!("couldn't run curl ({e}) — is it installed?")))?;
    // Feed the body from another thread so a large picture can't deadlock
    // against curl filling its stdout pipe.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let body = body.to_vec();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let out = child.wait_with_output().map_err(|e| CallError::Network(e.to_string()))?;
    let _ = writer.join();

    let stdout = out.stdout;
    let Some(at) = stdout.windows(STATUS_MARK.len()).rposition(|w| w == STATUS_MARK) else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(CallError::Network(if err.is_empty() { format!("curl exited with {}", out.status) } else { err }));
    };
    let code: u16 = String::from_utf8_lossy(&stdout[at + STATUS_MARK.len()..]).trim().parse().unwrap_or(0);
    let body = stdout[..at].to_vec();
    match code {
        200..=299 => Ok(body),
        0 => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(CallError::Network(if err.is_empty() { format!("no answer (curl exited with {})", out.status) } else { err }))
        }
        _ => Err(CallError::Status(code, String::from_utf8_lossy(&body).into_owned())),
    }
}

fn post_json<T: for<'de> Deserialize<'de>>(url: &str, auth: Option<&Path>, body: &Value, timeout: Duration) -> Result<T, CallError> {
    let bytes = post(url, auth, JSON, body.to_string().as_bytes(), timeout)?;
    serde_json::from_slice(&bytes).map_err(|e| CallError::BadAnswer(e.to_string()))
}

/// Write curl's config holding the token's header (root-only), once per token.
fn write_auth(token: &str) -> io::Result<PathBuf> {
    let dir = run_dir();
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let path = dir.join("uplink.curlrc");
    let text = format!("header = \"Authorization: Bearer {token}\"\n");
    if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        file.write_all(text.as_bytes())?;
    }
    Ok(path)
}

// ── state the status page shows ─────────────────────────────────────────────

/// The link's state for `/api/status` (and so for Barbie's Lab too).
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UplinkStatus {
    /// `hqcam connect` has been run.
    pub configured: bool,
    pub url: Option<String>,
    pub name: Option<String>,
    pub camera_id: Option<i64>,
    /// The last poll got through.
    pub connected: bool,
    /// Sending live frames right now.
    pub streaming: bool,
    pub last_contact_secs: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Uplink {
    status: Mutex<UplinkStatus>,
    last_contact: Mutex<Option<Instant>>,
    streaming: AtomicBool,
}

impl Uplink {
    pub fn status(&self) -> UplinkStatus {
        let mut s = self.status.lock().unwrap().clone();
        s.streaming = self.streaming.load(Ordering::Relaxed);
        s.last_contact_secs = self.last_contact.lock().unwrap().map(|t| t.elapsed().as_secs());
        s
    }

    fn update(&self, f: impl FnOnce(&mut UplinkStatus)) {
        f(&mut self.status.lock().unwrap());
    }

    fn contact(&self) {
        *self.last_contact.lock().unwrap() = Some(Instant::now());
        self.update(|s| {
            s.connected = true;
            s.last_error = None;
        });
    }

    fn failed(&self, error: impl fmt::Display) {
        let error = error.to_string();
        self.update(|s| {
            s.connected = false;
            s.last_error = Some(error);
        });
    }
}

// ── the link thread ─────────────────────────────────────────────────────────

/// Sleep up to `wait`, returning early if `uplink.conf` changes — so a
/// reconnect or disconnect takes effect at once, even mid back-off.
fn back_off(path: &Path, link: &Link, wait: Duration) {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        thread::sleep(CONF_CHECK.min(until.saturating_duration_since(Instant::now())));
        if Link::load(path).as_ref() != Some(link) {
            return;
        }
    }
}

#[derive(Deserialize)]
struct PollAnswer {
    #[serde(default)]
    commands: Vec<Job>,
    #[serde(default)]
    live: bool,
}

#[derive(Deserialize)]
struct Job {
    id: i64,
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct FrameAnswer {
    live: bool,
}

/// Start the link thread. Harmless without `uplink.conf`: it just checks for
/// one every few seconds.
pub fn start(app: App) {
    thread::spawn(move || run(app));
}

fn run(app: App) {
    let path = conf_path();
    let mut retry = RETRY_MIN;
    let mut logged_unconfigured = false;
    let mut last_link: Option<Link> = None;

    loop {
        let Some(link) = Link::load(&path) else {
            if !logged_unconfigured {
                log::info!("uplink: not connected to Barbie's Lab (run `sudo hqcam connect` to connect)");
                logged_unconfigured = true;
            }
            if last_link.take().is_some() {
                log::info!("uplink: disconnected");
            }
            app.uplink.update(|s| *s = UplinkStatus::default());
            thread::sleep(IDLE_CHECK);
            continue;
        };
        logged_unconfigured = false;
        if last_link.as_ref() != Some(&link) {
            log::info!("uplink: linked to {} as {}", link.url, link.name);
            app.uplink.update(|s| {
                s.configured = true;
                s.url = Some(link.url.clone());
                s.name = Some(link.name.clone());
                s.camera_id = link.camera_id;
            });
            last_link = Some(link.clone());
        }
        let auth = match write_auth(&link.token) {
            Ok(p) => p,
            Err(e) => {
                log::error!("uplink: can't write {}: {e}", run_dir().display());
                app.uplink.failed(format!("can't write {}: {e}", run_dir().display()));
                back_off(&path, &link, RETRY_MAX);
                continue;
            }
        };

        let streaming = app.uplink.streaming.load(Ordering::Relaxed);
        let body = json!({ "status": http::full_status(&app), "streaming": streaming });
        match post_json::<PollAnswer>(&format!("{}{PATH_POLL}", link.url), Some(&auth), &body, POLL_TIMEOUT) {
            Ok(answer) => {
                if !app.uplink.status().connected {
                    log::info!("uplink: connected to {}", link.url);
                }
                app.uplink.contact();
                retry = RETRY_MIN;
                for command in answer.commands {
                    handle(&app, &link, &auth, command);
                }
                if answer.live && !app.uplink.streaming.swap(true, Ordering::Relaxed) {
                    let (app, link, auth) = (app.clone(), link.clone(), auth.clone());
                    thread::spawn(move || stream(&app, &link, &auth));
                }
            }
            Err(e) if e.rejected() => {
                log::error!("uplink: {e} — the camera's access was removed; run `sudo hqcam connect` again");
                app.uplink.failed(format!("{e} — run `sudo hqcam connect` again"));
                back_off(&path, &link, REJECTED_WAIT);
            }
            Err(e) => {
                if app.uplink.status().connected {
                    log::warn!("uplink: {e}; retrying");
                }
                app.uplink.failed(&e);
                back_off(&path, &link, retry);
                retry = (retry * 2).min(RETRY_MAX);
            }
        }
    }
}

/// Carry out one command and report the outcome (a picture answers its own).
fn handle(app: &App, link: &Link, auth: &Path, command: Job) {
    log::info!("uplink: request #{} ({})", command.id, command.kind);
    let outcome: Result<(), String> = match command.kind.as_str() {
        "snapshot" => match app.snapshots.grab(&app.bus) {
            Ok(frame) => {
                let url = format!("{}{PATH_PICTURES}?command={}", link.url, command.id);
                match post(&url, Some(auth), JPEG, &frame, SEND_TIMEOUT) {
                    Ok(_) => return,
                    Err(e) => Err(format!("the picture didn't upload: {e}")),
                }
            }
            Err(e) => Err(e.to_string()),
        },
        "settings" => apply_settings(app, &command.payload),
        other => Err(format!("this camera doesn't know \"{other}\" — update hqcam")),
    };
    if let Err(e) = &outcome {
        log::warn!("uplink: request #{} failed: {e}", command.id);
    }
    let url = format!("{}{PATH_COMMANDS}/{}", link.url, command.id);
    let body = json!({ "error": outcome.err() });
    if let Err(e) = post(&url, Some(auth), JSON, body.to_string().as_bytes(), SEND_TIMEOUT) {
        log::warn!("uplink: couldn't report request #{}: {e}", command.id);
    }
}

/// `{ rotation?, hflip?, vflip?, lensPosition? }` from Barbie's Lab.
fn apply_settings(app: &App, payload: &Value) -> Result<(), String> {
    let get_bool = |key: &str| payload.get(key).and_then(Value::as_bool);
    let change = OrientationChange {
        rotation: payload.get("rotation").and_then(Value::as_u64).map(|r| r as u16),
        hflip: get_bool("hflip"),
        vflip: get_bool("vflip"),
    };
    if change.rotation.is_some() || change.hflip.is_some() || change.vflip.is_some() {
        settings::set_orientation(&app.cam, change).map_err(|e| e.to_string())?;
    }
    if let Some(lens) = payload.get("lensPosition") {
        let lens = settings::lens_from_json(lens).map_err(|e| e.to_string())?;
        settings::set_focus(&app.cam, lens.as_deref()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Live view: a frame about once a second until the server says nobody is
/// watching (or sending fails).
fn stream(app: &App, link: &Link, auth: &Path) {
    log::info!("uplink: someone is watching — sending live frames");
    let url = format!("{}{PATH_FRAME}", link.url);
    loop {
        let started = Instant::now();
        let Some(frame) = app.bus.next_frame(FRAME_WAIT) else {
            log::warn!("uplink: no frames from the camera; live view paused");
            break;
        };
        match post(&url, Some(auth), JPEG, &frame, SEND_TIMEOUT)
            .and_then(|b| serde_json::from_slice::<FrameAnswer>(&b).map_err(|e| CallError::BadAnswer(e.to_string())))
        {
            Ok(answer) if answer.live => {}
            Ok(_) => {
                log::info!("uplink: nobody watching any more — live frames stopped");
                break;
            }
            Err(e) => {
                log::warn!("uplink: live frame failed: {e}; stopping live view");
                break;
            }
        }
        if let Some(rest) = LIVE_INTERVAL.checked_sub(started.elapsed()) {
            thread::sleep(rest);
        }
    }
    app.uplink.streaming.store(false, Ordering::Relaxed);
}

// ── `hqcam connect` / `hqcam disconnect` ────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connected {
    camera_id: i64,
    name: String,
    token: String,
}

/// `hqcam connect [URL] [--name NAME] [--label LABEL]` — sign this camera in to
/// Barbie's Lab. Asks for the username and password (read from stdin lines when
/// not at a terminal, for scripting). Returns the process exit code.
pub fn connect_cli(args: &[String]) -> i32 {
    let mut url = DEFAULT_URL.to_string();
    let mut name: Option<String> = None;
    let mut label: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = it.next().cloned(),
            "--label" => label = it.next().cloned(),
            a if a.starts_with("http://") || a.starts_with("https://") => url = a.trim_end_matches('/').to_string(),
            other => {
                eprintln!("hqcam connect: unexpected \"{other}\"\n{}", usage());
                return 2;
            }
        }
    }
    let path = conf_path();
    if let Err(e) = check_writable(&path) {
        eprintln!("hqcam connect: can't write {}: {e} — run it with sudo", path.display());
        return 1;
    }

    let hostname = fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    let name = name.unwrap_or_else(|| hostname.clone());
    let label = label.unwrap_or_else(|| name.clone());
    println!("Connecting this camera (\"{label}\") to {url}");
    let Some(username) = ask("Barbie's Lab username: ", false) else { return 1 };
    let Some(password) = ask("Password: ", true) else { return 1 };

    let body = json!({ "username": username, "password": password, "camera": name, "label": label });
    let connected: Connected = match post_json(&format!("{url}{PATH_CONNECT}"), None, &body, SEND_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hqcam connect: {e}");
            return 1;
        }
    };
    let link = Link { url: url.clone(), token: connected.token, name: connected.name, camera_id: Some(connected.camera_id) };
    if let Err(e) = link.save(&path) {
        eprintln!("hqcam connect: signed in, but couldn't save {}: {e}", path.display());
        return 1;
    }
    println!(
        "Connected: camera #{} \"{label}\" ({}). It will show up on Barbie's Lab's Cameras screen within a few seconds.",
        connected.camera_id, link.name
    );
    0
}

/// `hqcam disconnect` — forget the link (the token stays valid on the server
/// until revoked on its Users screen).
pub fn disconnect_cli() -> i32 {
    let path = conf_path();
    match fs::remove_file(&path) {
        Ok(()) => {
            let _ = fs::remove_file(run_dir().join("uplink.curlrc"));
            println!("Disconnected from Barbie's Lab. To revoke its access there too, remove the device on the Users screen.");
            0
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("Not connected.");
            0
        }
        Err(e) => {
            eprintln!("hqcam disconnect: can't remove {}: {e} — run it with sudo", path.display());
            1
        }
    }
}

pub fn usage() -> &'static str {
    "usage:\n  hqcam                                   run the camera service\n  \
     sudo hqcam connect [URL] [--name NAME] [--label LABEL]\n                                          \
     link this camera to Barbie's Lab (default https://www.barbieslab.com)\n  \
     sudo hqcam disconnect                   forget that link"
}

fn check_writable(path: &Path) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir)?;
    let probe = dir.join(".hqcam-write-test");
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)
}

/// Prompt on a terminal (hiding the answer for passwords via `stty -echo`), or
/// read the next line of stdin when it isn't one.
fn ask(prompt: &str, secret: bool) -> Option<String> {
    let tty = io::stdin().is_terminal();
    if tty {
        print!("{prompt}");
        let _ = io::stdout().flush();
    }
    let stty = |arg: &str| {
        let _ = Command::new("stty").arg(arg).stdin(Stdio::inherit()).status();
    };
    if tty && secret {
        stty("-echo");
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    if tty && secret {
        stty("echo");
        println!();
    }
    match read {
        Ok(n) if n > 0 && !line.trim().is_empty() => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        _ => {
            eprintln!("hqcam connect: nothing entered");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_round_trips_and_is_root_only() {
        let dir = std::env::temp_dir().join(format!("hqcam-uplink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("uplink.conf");
        assert_eq!(Link::load(&path), None);
        let link = Link { url: "https://example.test".into(), token: "abc".into(), name: "cam-x".into(), camera_id: Some(3) };
        link.save(&path).unwrap();
        assert_eq!(Link::load(&path), Some(link));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Needs `curl` and a listening port; a real request through `post`.
    #[test]
    fn post_reads_status_and_body() {
        use std::io::{Read, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for reply in ["HTTP/1.1 200 OK", "HTTP/1.1 403 Forbidden"] {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let body = r#"{"error":"Your role does not allow this"}"#;
                write!(sock, "{reply}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let url = format!("http://127.0.0.1:{port}/x");
        let ok = post(&url, None, JSON, b"{}", Duration::from_secs(5)).unwrap();
        assert_eq!(ok, br#"{"error":"Your role does not allow this"}"#);
        let err = post(&url, None, JSON, b"{}", Duration::from_secs(5)).unwrap_err();
        assert!(err.rejected(), "{err}");
        server.join().unwrap();
        let down = post("http://127.0.0.1:1/x", None, JSON, b"{}", Duration::from_secs(5)).unwrap_err();
        assert!(!down.to_string().ends_with(": "), "{down}");
    }

    #[test]
    fn server_errors_read_well() {
        let e = CallError::Status(403, r#"{"error":"Your role does not allow this"}"#.into());
        assert_eq!(e.to_string(), "the server said 403: Your role does not allow this");
        assert!(e.rejected());
        assert!(!CallError::Network("timeout".into()).rejected());
    }
}
