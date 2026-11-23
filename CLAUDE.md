# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An MJPEG streaming server that runs on a Raspberry Pi (Zero), captures from an attached CSI ribbon camera, and broadcasts the feed over HTTP so it can be consumed by the "Motion Eye" system running on an HQ server. Both serve identically on port **6877** (`/index.html` + `/stream.mjpg`):

- **Rust (`src/`, primary)** — the in-progress port. Shells out to `rpicam-vid` for hardware MJPEG capture and rebroadcasts. See "Rust app" below.
- **Python (`fv_cam.py`, legacy)** — the original single-file `picamera2` server, kept as a reference.

The full offline pipeline is implemented: capture to a USB drive while away from HQ (see "USB footage recording"), then sync it to HQ storage and reclaim space on reconnect (see "Footage sync to HQ").

## Rust app (primary)

```bash
cargo check                 # typecheck
cargo build --release       # binary at target/release/hqcam
RUST_LOG=debug cargo run     # run locally (needs rpicam-vid + a CSI camera at runtime)
```

Cross-compile for the Pi Zero (ARMv6) rather than building on-device — on-target builds are painfully slow. Use `cross` or a `arm-unknown-linux-gnueabihf` toolchain.

Package for the apt repo with [`cargo-deb`](https://github.com/kornelski/cargo-deb): `cargo deb` reads `[package.metadata.deb]` in `Cargo.toml`, installs the binary to `/usr/local/bin/hqcam` and `_scripts/hqcam.service` to systemd, and runs `debian/postinst` to enable+start the service. The package depends on `rpicam-apps` (older Pi OS: `libcamera-apps`).

Naming: the binary/crate/package and the systemd service are all **`hqcam`** (`SyslogIdentifier=hqcam`). Manual (non-deb) install uses `_scripts/install-hqcam-service.sh` / `uninstall-hqcam-service.sh`, which manage the `hqcam` unit.

Tests are minimal: `cargo test` runs the motion-detector unit tests in `src/recorder/motion.rs` plus the capture-config / `rpicam-vid` argument tests in `src/cam/mod.rs`. The rest is I/O plumbing exercised on-device.

## Architecture (Rust)

- **`src/cam/mod.rs`** is the camera + plumbing. `start()` spawns a supervised thread that runs `rpicam-vid --codec mjpeg -o -` and restarts it if it dies. Its stdout is split into JPEG frames by scanning for `SOI`/`EOI` markers (`FFD8`…`FFD9`), and each frame is published to a `FrameBus` — a single-slot, sequence-numbered broadcast (`Mutex` + `Condvar`) that is the Rust equivalent of the Python `StreamingOutput`/`threading.Condition`. `MjpegStream` is a `Read` adapter that wraps the `FrameBus` as an endless `multipart/x-mixed-replace` body.
- **`src/main.rs`** wires the pieces together and runs the `tiny_http` accept loop (thread-per-request); **`src/http/mod.rs`** routes each request (see "Web UI + JSON API"). `/stream.mjpg` is a chunked `MjpegStream` response that runs until the client disconnects.
- **`src/recorder/mod.rs`** is a second `FrameBus` consumer (alongside `MjpegStream`) that saves footage to a USB drive while offline — see "USB footage recording" below.
- Capture size/rate/quality are env-configurable via `record.conf` (`HQCAM_WIDTH`, `HQCAM_HEIGHT`, `HQCAM_FRAMERATE`, `HQCAM_QUALITY`; `Capture` in `cam/mod.rs`, default 1296x972@15 q80 — sized for a Zero 2 W; a single-core Zero W needs ~640x480@10 q70). `HQCAM_TUNING_FILE` passes a libcamera tuning JSON (needed for NoIR modules, e.g. `imx219_noir.json`; missing file => ignored with a warning). `HQCAM_LENS_POSITION` (dioptres, `0` = infinity, or `default` = hyperfocal) locks focus on autofocus modules (Camera Module 3) via `--autofocus-mode manual --lens-position`; unset => rpicam-vid's own AF, invalid => ignored with a warning; leave unset on IMX219. Fixed `rpicam-vid` flags live in `RPICAM_BASE_ARGS`. Measured numbers are in the `Capture` doc comment.
- **`src/rotate/mod.rs`** — `HQCAM_ROTATION` is clockwise 0|90|180|270. 0/180 + flips go to `rpicam-vid` (free); libcamera can't do quarter turns, so 90/270 are a lossless per-frame `tjTransform` from `libturbojpeg.so.0`, `dlopen`ed at runtime (no cross-link; deb dep `libturbojpeg0`). ~94 ms/frame on a Zero 2 W at 1640x1232 → caps ~10 fps. Missing lib => frames pass through unrotated + an error log.

## Web UI + JSON API

`src/http/mod.rs` serves the UI (`src/http/ui/` — plain HTML/CSS/JS, embedded with `include_str!`) and a JSON API on the same port. The full route list is the doc comment at the top of that file. **LAN only, no login.**

- **Status** (`GET /api/status`): `src/status/` reads temp, `vcgencmd get_throttled` (decoded), load, memory, WiFi (`nmcli … --rescan no`, never forces a scan), plus camera/recorder/snapshot state. Unreadable sources are `null`.
- **Snapshots** (`src/snapshot/`): "Take a Pic" grabs the next live frame (no extra capture/encode; stream resolution). **Never saved on the SD card** (owner requirement): saved only to the first *mounted* target — `HQCAM_NETWORK_DIR` (network share, preferred because it's off the device), then the USB drive (`HQCAM_RECORD_DIR`) — under `<target>/snapshots/YYYY-MM-DD/<n>.jpg`, capped at `HQCAM_SNAPSHOT_MAX_MB` (200). With no target mounted, `POST /api/snapshots` is 409 and the UI uses `POST /api/snapshots/download` instead: the JPEG goes straight back to the requesting device from memory and nothing is written. Names are validated so requests can't escape the folder.
- **Manual record** (`POST /api/record {"on":true}`): `Recorder::set_manual` in `src/recorder/`. Always continuous, works online or offline, still USB-only (409 with no drive). The recorder thread now always runs; `HQCAM_RECORD=0` only disables the *automatic* offline mode.
- **Orientation / focus** (`POST /api/orientation`, `/api/focus`): applied live (capture restarts, ~1s blip) and **saved to `record.conf`** by `src/conf/` (replaces active `KEY=` lines, else uncomments the documented `#KEY=` line, else appends; atomic rename). The legacy `/orientation?…` query form still works but is in-memory only.

## Barbie's Lab link (`src/uplink/`)

Lets a camera be watched and driven from www.barbieslab.com (barbies-lab's `cameras` module, `~/dev/barbies-lab`) — **outbound only**, so it works from any home network without port forwarding or a VPN.

- `sudo hqcam connect [URL] [--name N] [--label L]` signs in once with a Barbie's Lab login (needs `cameras.control`; password used once, never stored) and writes the token to `/usr/local/etc/hqcam/uplink.conf` (0600; `HQCAM_UPLINK_CONF` overrides). Default URL `https://www.barbieslab.com` — **use www**: the bare domain 301-redirects, which turns POSTs into GETs. `sudo hqcam disconnect` removes it (revoke on barbies-lab's Users screen to kill the token itself). Username/password come from stdin lines when not on a TTY (scriptable).
- The service's uplink thread re-reads `uplink.conf` every round (connect/disconnect apply within seconds, even mid back-off) and **long-polls** `POST /api/cameras/feed/poll` with the full `/api/status` JSON. Commands: `snapshot` → next frame to `/feed/pictures?command=` (memory only — nothing on the SD card), `settings` → `src/settings/` (same code as the local UI, saved to record.conf). Live view: while the poll says `live`, ~1 frame/s to `/feed/frame` until it answers `live:false`; polls carry `streaming:true` meanwhile so they're still held.
- HTTP is **`curl`** (deb dep), not a TLS crate: pure-Rust cross-compile, small binary. The token reaches curl via a root-only `-K` config in `/run/hqcam/` (`HQCAM_RUN_DIR`), never argv/`ps`.
- 401/403 → logged "access was removed; run `sudo hqcam connect` again", 2 min back-off; network errors back off 5→60 s. State is in `/api/status` → `uplink` and on the local page.

Planned next: VPN-based backup to the SodaPop network drive.

## WiFi fallback hotspot (vehicle access)

> **DEACTIVATED (2026-11-20).** The AP was holding the radio and keeping cams from rejoining HQ WiFi. postinst no longer enables `hqcam-hotspot.timer`; it now disables/stops it and runs `hqcam-hotspot off` on upgrade. `firstrun.sh` no longer starts it. The script, units and config still ship, so it can be used by hand (`sudo hqcam-hotspot on|off|status`). To re-activate, restore the `enable`/`start` lines in postinst (they're left commented there). The rest of this section describes the design when it's active.

So the cam is viewable from a phone/laptop while it's in the vehicle (no HQ WiFi around), the Pi runs a **fallback access point**. A Pi has a single WiFi radio, so it is either a client *or* an AP, never both — the logic picks one:

- On boot (and re-checked every 2 min) it waits `AP_GRACE` seconds (default 90 — generous so a weak parking-lot signal has time to associate + get DHCP) for a known network. If HQ WiFi is in range it stays a normal client (Motion Eye keeps pulling the stream). If not, it brings up its own AP — cam at **`http://10.42.0.1:6877`**.
- While on the AP it periodically (every `AP_REJOIN_EVERY` s, default 600; 0 disables) drops the AP for a brief window (`AP_REJOIN_GRACE`, default 20s) to look for HQ and rejoin on its own — so parking at HQ after roaming reconnects without a reboot. A single radio can't scan while running the AP, hence the brief drop; it's **skipped while a client is associated to the AP** (`AP_REJOIN_SKIP_IF_CLIENTS=yes`, detected via `iw station dump`) so an active viewer isn't interrupted. You can still force it: `hqcam-hotspot off`.
- Because `AP_GRACE` (90s) exceeds systemd's default 90s oneshot start timeout, `hqcam-hotspot.service` sets `TimeoutStartSec=180`. Raise it if you raise `AP_GRACE`.
- NetworkManager only (Bookworm/Trixie). The AP profile uses `ipv4.method shared`, so NM runs its own DHCP+DNS on `10.42.0.0/24` — no hostapd/dnsmasq config. Hence the `network-manager` + `dnsmasq-base` deb deps.
- **WiFi power-save is disabled** via a shipped drop-in (`_scripts/wifi-powersave-off.conf` → `/etc/NetworkManager/conf.d/`, `wifi.powersave=2`), because Pi OS's default power-save makes the weak onboard radio drop/fail marginal links. postinst reloads NetworkManager to apply it. Reg-domain/TX-power is a device-side `raspi-config` setting, not shipped.

Pieces (all in `_scripts/`, shipped by the deb):
- `hqcam-hotspot.sh` → `/usr/local/bin/hqcam-hotspot` — the toggle logic. CLI: `hqcam-hotspot {auto|on|off|status}`.
- `hqcam-hotspot.service` (oneshot decision run) + `hqcam-hotspot.timer` (boot + periodic). The **timer** is what's enabled; it drives the service.
- `hotspot.conf` → installed once to `/usr/local/etc/hqcam/hotspot.conf` (ships as `hotspot.conf.default`; postinst copies it only if absent, so an edited SSID/password survives upgrades). **Set `AP_SSID`/`AP_PASSWORD` here**, then `systemctl restart hqcam-hotspot.service`.

Manual (non-deb) install: `_scripts/install-hqcam-hotspot.sh` (run as root from `_scripts/`), mirroring `install-hqcam-service.sh`.

Note: the deb ships ONE capture binary per arch, staged as `hqcam-<arch>` (built against the glibc 2.24 floor by `_package/_release.sh`; glibc is backward compatible). `postinst` moves it to `/usr/local/bin/hqcam`. `preinst`/`postinst` clean staged binaries by explicit name (`hqcam-arm64`, `hqcam-armhf`, legacy per-glibc `hqcam-*-2.*`) — never `hqcam-*` — so the sibling `hqcam-hotspot` / `hqcam-sync` commands aren't deleted on upgrade.

Release builds run from a fresh clone (the admin CI/CD "HQ Cams" tab → sp-builder on Frasier, default branch `edge`): only `_package/hqcam/DEBIAN/` is tracked; the rest of `_package/hqcam/` is staged at build time. `_release.sh` takes `<major|minor|patch|none|X.Y.Z> <deploy> <arm64|armhf|all> <rc>`.

## USB footage recording (offline)

So footage isn't lost when the Pi is away from HQ (Motion Eye can't reach it), `src/recorder/mod.rs` records the live stream to an attached USB drive. It is **built into the `hqcam` binary** — a second `FrameBus` consumer started from `main.rs`, not a separate service — and self-gates, so it's a no-op on the HQ network or with no drive attached.

- **Only records when offline** (`is_offline()` reuses the hotspot's nmcli check: no active client WiFi that isn't our own AP) **and** the target is a real mountpoint. **Never writes to the SD card** — `mount_ready()` compares `st_dev` against the parent dir, so a missing/unmounted drive means "skip", never "fill the OS disk".
- **Raw MJPEG** files (concatenated JPEGs straight off the bus — no re-encode, no container to finalize). Chosen for power-loss resilience in a vehicle: an abrupt cut truncates only the final frame. Playable directly in VLC/ffmpeg; transcode later with `ffmpeg -c copy`.
- **Date folders, numbered segments** (`/media/hqcam/YYYY-MM-DD/1.mjpeg`, …), rotated every `HQCAM_SEGMENT_SECS` (default 300). **Ring buffer:** oldest segments are pruned (`df`-based) when free space drops below `HQCAM_MIN_FREE_MB` (default 512).
- Dates come from the Pi's wall clock (UTC), which may be wrong while offline with no NTP — footage is still saved, just possibly under an earlier date.

Two capture modes, chosen by `HQCAM_MOTION_GATED` (default off):
- **Continuous** (default) — `record_continuous_segment()` writes every frame while offline. Never misses anything; fills the drive faster (the ring buffer keeps it bounded).
- **Motion-gated** — `record_motion_session()` records only around motion. `src/recorder/motion.rs` samples frames at `HQCAM_MOTION_FPS` (default 3), decodes each at ~1/8 scale via `jpeg-decoder` (DCT downscale — the Huffman pass still runs, so it's ~20× cheaper than full-res but not free; drop to 1–2 fps on a single-core original Pi Zero), averages to a 40×30 luma grid, and flags motion when > `HQCAM_MOTION_SENSITIVITY` of cells change by > `HQCAM_MOTION_CELL_THRESHOLD`. A **pre-roll** ring (`HQCAM_PREROLL_SECS`) prepends the lead-up; recording continues `HQCAM_MOTION_TIMEOUT`s past the last motion (post-roll). Both modes share the `Segment` writer.

Pieces:
- `src/recorder/mod.rs` — orchestration + the `Segment` writer + gating. Tunables are env vars (all with defaults): `HQCAM_RECORD_DIR`, `HQCAM_SEGMENT_SECS`, `HQCAM_MIN_FREE_MB`, `HQCAM_WIFI_IFACE`, `HQCAM_AP_CON`, `HQCAM_CHECK_SECS`, plus the motion knobs above.
- `src/recorder/motion.rs` — the sampled, DCT-scaled, coarse-grid motion detector. Has unit tests (`cargo test`) covering the real decode of `test.jpg` and the grid-diff thresholds.
- `_scripts/record.conf` → `/usr/local/etc/hqcam/record.conf` — optional `EnvironmentFile` for the `hqcam` service (ships as `record.conf.default`; postinst installs it only if absent). Wired in via `EnvironmentFile=-…` in `hqcam.service`.
- `_scripts/media-hqcam.mount` → `/etc/systemd/system/` — mounts a USB drive **labelled `HQCAM`** at `/media/hqcam` (`nofail`, best-effort). Enabled by postinst.

New crate dep: `jpeg-decoder` (pure Rust, `default-features = false` to drop rayon — cross-compiles for ARM). No new deb deps (still just `nmcli`/`df`).

## Footage sync to HQ

The counterpart to offline recording: when the Pi is back on a network that can reach HQ storage, push the recorded clips off the USB drive and delete the local copies to reclaim space. Unlike the recorder (Rust, in the binary), sync is a **standalone shell tool** (`rsync` is a shell tool anyway), mirroring the hotspot's script + service + timer shape.

- **Race-free with the recorder by construction:** the recorder writes only while *offline*; sync transfers only when it can *reach HQ*. Belt-and-braces, the recorder writes `<n>.mjpeg.part` and renames to `<n>.mjpeg` only on clean close (`Segment::close`), and sync transfers `*.mjpeg` only. A power-cut `.part` orphan is recovered (renamed) by sync after `SYNC_MIN_AGE`.
- **rsync-over-ssh**, `-a --partial --append-verify --remove-source-files`: interrupted transfers resume and source files are removed only once fully sent, so leaving WiFi range mid-sync never loses footage. Empty date folders are pruned after.
- **Gated**: opt-in (`SYNC_ENABLED=yes`), needs `SYNC_DEST` set, the drive mounted, and a TCP reachability pre-check to the dest host (which naturally excludes AP mode / being off HQ).

Pieces (all `_scripts/`, shipped by the deb):
- `hqcam-sync.sh` → `/usr/local/bin/hqcam-sync`. CLI: `hqcam-sync {auto|now|status}`.
- `hqcam-sync.service` (oneshot, `TimeoutStartSec=3600`) + `hqcam-sync.timer` (boot + every 5 min). The **timer** is what's enabled.
- `sync.conf` → `/usr/local/etc/hqcam/sync.conf` (ships as `sync.conf.default`, installed once). Set `SYNC_DEST` + `SYNC_ENABLED` + an SSH key here.

New deb deps: `rsync`, `openssh-client`. Manual install: `_scripts/install-hqcam-sync.sh`.

## Legacy Python app

```bash
pip3 install simplejpeg picamera2   # on the Pi
python3 fv_cam.py                    # serves on port 6877
```

`fv_cam.py` is the whole legacy app: `StreamingOutput` + `threading.Condition` feed `/stream.mjpg` via `picamera2`'s hardware `JpegEncoder`. Its old `farmview` systemd unit and install scripts have been removed in favour of the `hqcam` service; to run the Python version now, launch it manually.

## Language: prefer Rust over Python

Python is only the current default because it was the path of least resistance when the project was spun up. **Rust is the preferred language** and new work should move toward Rust wherever feasible — a key motivation is that Rust enables distribution as a `.deb` package via the owner's APT repo.

When written in Rust, structure the project by domain object: each domain gets its own folder containing a `mod.rs`, with `main.rs` at the `src/` root. For example:

```
src/
  cam/
    mod.rs
  main.rs
```

## Other conventions and constraints (from the project owner)

- Captured media uses date-named folders with numerically incremented files (e.g. `1.jpg`, `2.mov`).
- Keep the HTML simple and clean. If interactivity is needed use plain **JavaScript only — no TypeScript, no React**. Shoelace is acceptable for styling.

## Known inconsistencies to be aware of

- The header comment in `fv_cam.py` says port 7123, but the server actually binds 6877.
