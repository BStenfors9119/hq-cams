# hqcam — Access Guide

How to reach the camera and the Pi it runs on, in every situation. The camera
serves an MJPEG stream on **port 6877** whether it's on the HQ network or
broadcasting its own hotspot — only the address you point at changes.

- [Quick reference](#quick-reference)
- [1. Viewing the camera](#1-viewing-the-camera)
  - [On the HQ network](#on-the-hq-network)
  - [Away from HQ — the fallback hotspot](#away-from-hq--the-fallback-hotspot)
- [2. HTTP endpoints](#2-http-endpoints)
- [3. Viewing from specific clients](#3-viewing-from-specific-clients)
- [4. Reaching the Pi's shell (SSH)](#4-reaching-the-pis-shell-ssh)
- [5. Managing the camera service](#5-managing-the-camera-service)
- [6. Managing the hotspot](#6-managing-the-hotspot)
- [7. Recorded footage (offline recording)](#7-recorded-footage-offline-recording)
- [8. Configuration files](#8-configuration-files)
- [9. Installing / updating](#9-installing--updating)
- [10. Troubleshooting](#10-troubleshooting)

---

## Quick reference

| Situation | URL to open |
|---|---|
| On HQ WiFi | `http://<pi-hostname-or-ip>:6877/` |
| On the camera's own hotspot | `http://10.42.0.1:6877/` |
| Raw stream (VLC / Motion Eye) | `http://<host>:6877/stream.mjpg` |

| Task | Command (on the Pi) |
|---|---|
| Is it a client or an AP right now? | `hqcam-hotspot status` |
| Force the hotspot on | `sudo hqcam-hotspot on` |
| Turn the hotspot off (rejoin HQ) | `sudo hqcam-hotspot off` |
| Camera service state | `systemctl status hqcam` |
| Live camera logs | `journalctl -u hqcam -f` |
| Is footage being recorded? | `journalctl -u hqcam -f` (look for `recorder:` lines) |
| Where footage is saved | `/media/hqcam/<date>/<n>.mjpeg` (USB drive) |
| Footage sync status | `hqcam-sync status` |
| Force a footage sync now | `sudo hqcam-sync now` |

Default hotspot network: SSID **`hqcam`**, password **`hqcam1234`** — change
this before deployment (see [§8](#8-configuration-files)).

---

## 1. Viewing the camera

The Pi has a single WiFi radio, so it is **either** a client on an existing
network **or** broadcasting its own access point — never both at once. It picks
automatically (see [§6](#6-managing-the-hotspot)), which is why there are two
ways to view it.

### On the HQ network

When HQ WiFi is in range, the Pi joins it as a normal client and the HQ Motion
Eye system pulls the stream automatically. To view it directly from any device
on the same network:

```
http://<pi-hostname-or-ip>:6877/
```

Find the Pi's address from your router's DHCP leases, or if mDNS is available
try its `.local` hostname (e.g. `http://hqcam.local:6877/`).

### Away from HQ — the fallback hotspot

When no known WiFi is in range (e.g. the camera is out in the vehicle), the Pi
brings up **its own WiFi access point** so a phone or laptop can connect and
watch directly. No HQ network, router, or internet is involved.

1. On your phone/laptop, open WiFi settings and join the camera's network:
   - **SSID:** `hqcam` (unless changed in `hotspot.conf`)
   - **Password:** `hqcam1234` (unless changed — you should change it)
2. Once connected, open:
   ```
   http://10.42.0.1:6877/
   ```

`10.42.0.1` is always the camera's address on its own hotspot — NetworkManager
hands your device a `10.42.0.x` lease over its built-in DHCP, so the address
never changes.

To make the Pi rejoin HQ WiFi after it's been on the hotspot, either reboot it
while HQ WiFi is in range, or run `sudo hqcam-hotspot off` (see
[§6](#6-managing-the-hotspot)).

---

## 2. HTTP endpoints

The server (`hqcam`, an MJPEG server written in Rust) answers **GET** on
port 6877. Any other method returns `405`.

| Path | Response |
|---|---|
| `/` | `301` redirect to `/index.html` |
| `/index.html` | Minimal HTML page with an `<img src="stream.mjpg">` |
| `/stream.mjpg` | `multipart/x-mixed-replace` MJPEG stream, runs until the client disconnects |
| anything else | `404` |

The stream is chunked with no `Content-Length`; each JPEG frame is captured from
the CSI camera via `rpicam-vid` at 1296×972, 15 fps, quality 80.

---

## 3. Viewing from specific clients

**Web browser** — open `http://<host>:6877/` (the `<img>` page) or point
straight at `http://<host>:6877/stream.mjpg`.

**VLC** — Media → Open Network Stream → `http://<host>:6877/stream.mjpg`.

**Motion Eye (HQ)** — add as a *Network Camera* / *Simple MJPEG Camera* with the
stream URL `http://<pi-ip>:6877/stream.mjpg`. This is how HQ ingests the feed
when the Pi is on the HQ network.

**ffmpeg / recording** — for example, to grab a still:
```bash
ffmpeg -i http://<host>:6877/stream.mjpg -frames:v 1 snapshot.jpg
```

Replace `<host>` with the Pi's HQ address, or `10.42.0.1` when you're on its
hotspot.

---

## 4. Reaching the Pi's shell (SSH)

To administer the device itself (change config, view logs, toggle the hotspot):

- **On HQ WiFi:** `ssh <user>@<pi-hostname-or-ip>`
- **On the camera's hotspot:** join the `hqcam` network, then
  `ssh <user>@10.42.0.1`

(The camera stream and SSH share the same address in each case — only the port
differs: 6877 for the stream, 22 for SSH.)

Use the account and key/password configured on the Pi image. From that shell you
can run every command in the sections below.

---

## 5. Managing the camera service

The stream runs as the **`hqcam`** systemd service (`/usr/local/bin/hqcam`,
started by `/etc/systemd/system/hqcam.service`). It restarts automatically if it
crashes.

```bash
systemctl status hqcam         # running? since when?
sudo systemctl restart hqcam   # restart the stream
sudo systemctl stop hqcam      # stop it
journalctl -u hqcam -f         # follow logs live
journalctl -u hqcam -n 100     # last 100 log lines
```

For more verbose logs, the service sets `RUST_LOG=info`; raise it with
`sudo systemctl edit hqcam` and an override of `Environment=RUST_LOG=debug`.

---

## 6. Managing the hotspot

The client-vs-AP decision is made by `/usr/local/bin/hqcam-hotspot`, driven at
boot and every 2 minutes by `hqcam-hotspot.timer` → `hqcam-hotspot.service`.

**How the automatic decision works:**
- If a known WiFi (e.g. HQ) is already connected → stay a client, no AP.
- If disconnected → wait up to `AP_GRACE` seconds (default 90) for a known
  network to autoconnect; if none appears, bring up the AP. The grace is
  generous on purpose: a weak parking-lot signal needs time to associate and get
  a DHCP lease, and bailing to the AP too early strands the Pi on its own net.
- If the AP is already up → every `AP_REJOIN_EVERY` seconds (default 600) it
  briefly drops the AP to look for HQ and rejoins on its own, so parking at HQ
  after being away reconnects without a reboot. This is **skipped while someone
  is connected to the AP**, so a live viewer isn't interrupted. Set
  `AP_REJOIN_EVERY=0` to disable and hold the AP indefinitely instead.

**Manual control:**
```bash
hqcam-hotspot status     # client:<ssid> | ap:hqcam (http://10.42.0.1:6877) | disconnected
sudo hqcam-hotspot on    # force the AP up now
sudo hqcam-hotspot off   # drop the AP; NM rejoins a known network if in range
sudo hqcam-hotspot auto  # re-run the automatic decision
```

**The timer itself:**
```bash
systemctl status hqcam-hotspot.timer
journalctl -u hqcam-hotspot -f          # see each decision as it's made
```

Requires NetworkManager (Raspberry Pi OS Bookworm/Trixie). The AP uses NM's
`ipv4.method shared`, which provides DHCP + DNS on `10.42.0.0/24` — there is no
hostapd or dnsmasq config to manage.

---

## 7. Recorded footage (offline recording)

While the camera is **off the HQ network** — so Motion Eye can't pull and store
the feed — the Pi records the live stream to an **attached USB drive** so the
footage isn't lost. On HQ WiFi it records nothing and lets HQ own recording.
This is built into the `hqcam` binary; there's no separate service.

**How it behaves:**
- Records only when *offline* (disconnected, or running its own hotspot) **and**
  a USB drive is mounted at `/media/hqcam`.
- **Never writes to the SD card** — if no drive is mounted, it just skips saving
  (so a missing drive can't fill the OS disk).
- Files are **raw MJPEG** segments, rotated every 5 minutes, in date folders:
  `/media/hqcam/YYYY-MM-DD/1.mjpeg`, `2.mjpeg`, …
- **Ring buffer:** when the drive gets low (default under 512 MB free), the
  oldest segments are deleted to make room. Footage is overwritten oldest-first,
  never unbounded.

**Continuous vs. motion-gated** (`HQCAM_MOTION_GATED`, default off):
- **Continuous** (default) records everything while offline — never misses a
  thing, fills the drive faster (the ring buffer keeps it bounded).
- **Motion-gated** records *only around motion*, so the drive holds events
  instead of an empty scene. Each clip includes a few seconds of pre-roll
  (before the motion) and post-roll (after it stops). It analyses frames
  on-device, which costs some CPU — fine on a Pi Zero 2 W; on the original
  single-core Pi Zero lower `HQCAM_MOTION_FPS` to 1–2. Tune it in
  [§8](#8-configuration-files). If motion tuning ever feels unreliable, the safe
  fallback is continuous.

**Set up the drive (one time):**
1. Format a USB drive (ext4 recommended; FAT/exFAT also fine).
2. Label it **`HQCAM`** so it auto-mounts:
   ```bash
   sudo e2label /dev/sda1 HQCAM      # ext4
   # or: sudo fatlabel /dev/sda1 HQCAM   (FAT)
   ```
3. Plug it into the Pi and reboot (or `sudo systemctl start media-hqcam.mount`).
   Confirm it mounted: `mountpoint /media/hqcam`.

**Retrieve footage** — copy it off over SSH (see [§4](#4-reaching-the-pis-shell-ssh)),
or pull the drive and read it on any computer:
```bash
# from another machine, over SSH:
scp -r <user>@<pi-host>:/media/hqcam/2026-07-05 ./
```

**Play a `.mjpeg` file** — open it directly in VLC, or transcode losslessly to
a normal video (no re-encode, so it's fast even off-Pi):
```bash
ffmpeg -framerate 15 -i 1.mjpeg -c:v copy 1.mkv
```

**Check it's working:**
```bash
journalctl -u hqcam -f          # "recorder: recording to /media/hqcam/…" while offline
df -h /media/hqcam              # drive space
ls -R /media/hqcam              # saved segments
```

> **Time note:** date folders use the Pi's clock. If it's been offline with no
> internet, the clock may be behind until it next syncs time (NTP) on the HQ
> network — footage is still saved, just possibly under an earlier date.

Tuning (segment length, free-space floor, mount path) lives in
`/usr/local/etc/hqcam/record.conf` — see [§8](#8-configuration-files).

### Auto-sync to HQ

Instead of pulling footage off by hand, the Pi can **push it to HQ storage
automatically** whenever it's back on a network that can reach HQ, then delete
the local copies to free the drive. It's **opt-in** and off until configured.

**How it behaves:**
- Runs only when it can reach the configured HQ host (so never over the vehicle
  hotspot, and never while the recorder is writing — the two states are mutually
  exclusive). Checks every 5 minutes; each check is a no-op if there's nothing to
  send or HQ is unreachable.
- Transfers finalized clips via `rsync` over SSH, resumes cleanly if the link
  drops mid-transfer, and removes each local file only after it's fully sent — so
  leaving WiFi range never loses footage.

**Set it up (one time):**
1. On the Pi, make a key and authorise it on the HQ host:
   ```bash
   sudo ssh-keygen -t ed25519 -f /usr/local/etc/hqcam/sync_id -N ""
   ssh-copy-id -i /usr/local/etc/hqcam/sync_id.pub hqcam@<hq-host>
   ```
2. Edit `/usr/local/etc/hqcam/sync.conf`: set `SYNC_ENABLED=yes`, `SYNC_DEST`
   (e.g. `hqcam@<hq-host>:/srv/hqcam-footage/`), and `SYNC_SSH_KEY`
   (`/usr/local/etc/hqcam/sync_id`). See [§8](#8-configuration-files).
3. Apply: `sudo systemctl restart hqcam-sync.timer`.

**Check it:**
```bash
hqcam-sync status               # enabled? dest? how many clips pending?
sudo hqcam-sync now             # force a sync run right now
journalctl -u hqcam-sync -f     # watch transfers / skips
```

---

## 8. Configuration files

**Hotspot settings** — `/usr/local/etc/hqcam/hotspot.conf`:

```sh
AP_SSID="hqcam"          # network name the Pi broadcasts
AP_PASSWORD="hqcam1234"  # WPA2 passphrase, min 8 chars — CHANGE THIS
AP_IFACE="wlan0"         # WiFi interface (wlan0 on every current Pi)
AP_CON_NAME="hqcam-ap"   # NetworkManager profile name (rarely changed)
AP_GRACE=90              # seconds to wait for known WiFi before AP fallback
AP_REJOIN_EVERY=600      # while on AP, retry HQ this often (0 = never)
AP_REJOIN_GRACE=20       # seconds to look for HQ during a retry
AP_REJOIN_SKIP_IF_CLIENTS=yes  # don't retry while a viewer is on the AP
```

After editing, apply with:
```bash
sudo systemctl restart hqcam-hotspot.service
```

The `.deb` ships this as `hotspot.conf.default` and only installs it as
`hotspot.conf` on **first** install, so an edited SSID/password survives
package upgrades. (This also means **existing devices keep the old
`AP_GRACE=25`** after an upgrade — bump it by hand on those.) If you raise
`AP_GRACE` above ~170, also raise `TimeoutStartSec` in
`hqcam-hotspot.service` so the decision run isn't killed early.

> **Change the default password before deployment.** `hqcam1234` is a
> placeholder and gives anyone in range access to the camera.

**WiFi reliability at range** is improved by a shipped drop-in
(`/etc/NetworkManager/conf.d/wifi-powersave-off.conf`) that disables the radio's
power-save. Two more device-side settings help and are *not* shipped: set the
correct WiFi country for full TX power (`sudo raspi-config` → Localisation →
WLAN Country), and — for real parking-lot range — a USB WiFi adapter with an
external antenna (the Pi Zero's onboard antenna is the hard limit). See
[§10](#10-troubleshooting).

**Recorder settings** — `/usr/local/etc/hqcam/record.conf` (all optional; every
value has a built-in default):

```sh
HQCAM_RECORD_DIR=/media/hqcam   # USB mount to record onto (must be a mountpoint)
HQCAM_SEGMENT_SECS=300          # seconds per segment file before rotating
HQCAM_MIN_FREE_MB=512           # keep this much free; prune oldest below it
HQCAM_WIFI_IFACE=wlan0          # match hotspot.conf if changed
HQCAM_AP_CON=hqcam-ap           # match hotspot.conf if changed

# Motion-gated recording (off by default = continuous):
HQCAM_MOTION_GATED=false        # true = record only around motion
HQCAM_MOTION_FPS=3              # frames/sec analysed (1–2 on original Pi Zero)
HQCAM_MOTION_SENSITIVITY=0.02   # fraction of frame that must change (lower = more sensitive)
HQCAM_MOTION_CELL_THRESHOLD=18  # per-cell brightness delta counted as changed (raise if noisy)
HQCAM_MOTION_TIMEOUT=10         # keep recording this long after motion stops (post-roll)
HQCAM_PREROLL_SECS=3            # seconds kept before a trigger (pre-roll)
```

Apply changes with `sudo systemctl restart hqcam`. Like the hotspot config, it
ships as `record.conf.default` and installs once, so edits survive upgrades.

**Sync settings** — `/usr/local/etc/hqcam/sync.conf` (opt-in; off until set):

```sh
SYNC_ENABLED=no                 # yes to turn on footage sync to HQ
SYNC_DEST=""                    # rsync dest, e.g. hqcam@10.24.3.20:/srv/hqcam-footage/
SYNC_SOURCE=/media/hqcam        # local footage (matches HQCAM_RECORD_DIR)
SYNC_SSH_KEY=""                 # ssh identity file for non-interactive auth
SYNC_SSH_PORT=22                # ssh port (also the reachability pre-check)
SYNC_MIN_AGE=30                 # don't touch clips modified within N seconds
SYNC_DELETE_AFTER=yes           # delete local copies after they transfer
SYNC_BW_LIMIT=0                 # upload cap in KB/s (0 = unlimited)
```

Apply with `sudo systemctl restart hqcam-sync.timer`. Ships as
`sync.conf.default` and installs once, so your dest survives upgrades. Setup
walkthrough is in [§7 → Auto-sync to HQ](#auto-sync-to-hq).

**Camera tuning** (resolution, framerate, quality) is compile-time, in the
`RPICAM_ARGS` constant in `src/cam/mod.rs`. Changing it means rebuilding the
binary.

---

## 9. Installing / updating

The camera installs from the SodaPop APT repo as the `hqcam` package.

```bash
# One-time: add the repo (from the cams/_tooling helper)
sudo ./add-sp-hqcam-repo.sh

# Install / update
sudo apt update
sudo apt install hqcam
```

The package installs the `hqcam` binary and service **and** the hotspot script,
units, timer, configs, and the USB footage-drive mount unit — enabling
everything on install. Dependencies: `rpicam-apps` (older Pi OS:
`libcamera-apps`), `network-manager`, `dnsmasq-base`.

For manual (non-deb) installs from a checkout, run as root from `_scripts/`:
```bash
sudo ./install-hqcam-service.sh       # camera stream service (+ recorder)
sudo ./install-hqcam-hotspot.sh       # fallback hotspot
sudo ./install-hqcam-sync.sh          # footage sync to HQ (needs rsync + ssh)
# recorder USB mount (optional): label a drive HQCAM, then:
sudo cp media-hqcam.mount /etc/systemd/system/ && sudo systemctl enable --now media-hqcam.mount
```

Building and packaging the binary is covered in the repo's top-level
`CLAUDE.md`.

---

## 10. Troubleshooting

**Can't reach `http://<host>:6877/`**
- Confirm which mode the Pi is in: `hqcam-hotspot status`. If it's `ap:...`,
  you must be joined to the `hqcam` network and use `10.42.0.1`, not the HQ
  address.
- Is the service up? `systemctl status hqcam`. Check logs with
  `journalctl -u hqcam -n 50`.

**Stream page loads but the image is black/broken**
- The capture subprocess (`rpicam-vid`) may be failing — check
  `journalctl -u hqcam -f` for restart loops. Confirm the CSI camera is seated
  and detected (`rpicam-hello --list-cameras`).

**The hotspot won't appear**
- `journalctl -u hqcam-hotspot -n 50` shows each decision. If a known WiFi is in
  range the Pi intentionally stays a client — force it with
  `sudo hqcam-hotspot on`.
- Confirm NetworkManager owns `wlan0` (`nmcli device status`).

**It went to the hotspot and won't rejoin HQ WiFi**
- It now retries HQ on its own every `AP_REJOIN_EVERY` seconds (default 600) —
  wait a cycle, or force it immediately with `sudo hqcam-hotspot off`. If it
  never rejoins, HQ likely isn't reachable from where the Pi is (see below), or
  `AP_REJOIN_EVERY=0` (disabled) in `hotspot.conf`.

**The Pi won't connect to HQ WiFi from far away (e.g. the parking lot), though a
phone can**
- The Pi's onboard antenna is much weaker than a phone's — some of the gap is
  reclaimable in software, but not all.
- Check power-save is off: `iw dev wlan0 get power_save` (should say "off"; the
  shipped drop-in sets this — `sudo systemctl reload NetworkManager` if it
  still says on).
- Check the regulatory domain so the radio runs at full legal power:
  `iw reg get` (should be your country, not `country 00`). Set it with
  `sudo raspi-config` → Localisation → WLAN Country.
- Measure the actual signal from that spot: `nmcli -f SSID,CHAN,SIGNAL device wifi`.
  A very low or absent HQ signal means the fix is on the HQ side (better 2.4 GHz
  coverage) or a USB WiFi adapter with an external antenna — the Pi Zero is
  2.4 GHz-only and has no external-antenna connector.
- Give a weak link time: a marginal join can take longer than the default
  `AP_GRACE`. It's already raised to 90s; raise it further in `hotspot.conf` if
  needed (and bump `TimeoutStartSec` in the service to match).

**No footage is being recorded while offline**
- Recording only happens when *offline*. On HQ WiFi it's intentionally idle.
- Confirm the drive is mounted: `mountpoint /media/hqcam` (must say "is a
  mountpoint"). If not, check the label is `HQCAM` and
  `systemctl status media-hqcam.mount`.
- Watch the decision live: `journalctl -u hqcam -f` — look for `recorder:`
  lines. "no USB drive mounted" means the drive isn't seen; "recording to …"
  means it's working.

**Footage isn't syncing to HQ**
- Check config: `hqcam-sync status` (is it enabled? is a dest set? how many clips
  pending?). Sync is off until `SYNC_ENABLED=yes` and `SYNC_DEST` are set.
- It only runs when it can reach the HQ host — confirm the Pi is on HQ WiFi and
  the host/port in `SYNC_DEST` is reachable. Force a run: `sudo hqcam-sync now`.
- Watch it: `journalctl -u hqcam-sync -f`. "not reachable" means it can't see HQ
  storage; SSH errors usually mean the key isn't authorised on the HQ host
  (`SYNC_SSH_KEY` + `ssh-copy-id`).
- Clips ending in `.mjpeg.part` are unfinished (a power cut mid-recording); sync
  recovers them automatically after `SYNC_MIN_AGE` seconds.
