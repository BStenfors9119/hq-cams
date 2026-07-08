# Task plan: dedicated `hqcam` apt pool/component

Goal: distribute the `hqcam` package (Rust `hqcam` binary + systemd unit) through the
existing SodaPop apt repo on the `tsradmin` server, but **segregated from the TSR receiver
packages** so the two device classes have independent indices, dependencies, and version
lines. Same server, same signing key, same `stable`/`testing` dists — separate **component**.

## Design decision

Use a new **component** named `hqcam` inside the existing `stable` (and `testing`) distribution,
rather than a whole separate `dists/hqcam/` distribution.

- Reuses the existing `Release`/`InRelease` signing flow unchanged (one signed `Release` covers
  all components in the dist).
- Clients add the component to their existing sources line: `… stable main hqcam`.
- Less duplication than a parallel distribution.

The separation is real because `dpkg-scanpackages` is pointed at a **per-component pool path**
(`pool/hqcam/`) instead of the whole `pool/`. Today the TSR scan globs all of `pool/`, so without
this the hqcam debs would fall into `main`.

### Open question to confirm before building
- **Target architectures.** Pi Zero W is ARMv6 (`armhf`/`armv6`); Pi Zero 2 W is `arm64`-capable.
  Plan assumes **`armhf` + `arm64`** (add `armv6` if any classic Zero W is in the fleet). No `amd64`.

---

## 1. Server-side repo layout (on `tsradmin@<APT_SERVER_IP>`)

New pool path, parallel to the existing TSR debs:

```
/home/tsradmin/sodapop/apt-repo/
  pool/
    main/            # existing TSR (stable)
    testing/         # existing TSR (testing)
    hqcam/           # NEW — hqcam stable debs land here
    hqcam-testing/   # NEW — hqcam testing debs (top-level, NOT under pool/testing,
                     #       so the recursive `pool/testing` scan can't catch them)
  dists/
    stable/
      main/binary-<arch>/...      # existing
      hqcam/binary-<arch>/...     # NEW
    testing/
      main/binary-<arch>/...      # existing
      hqcam/binary-<arch>/...     # NEW
```

> Keep `pool/hqcam/` a distinct directory tree so the component scan can target it precisely.

---

## 2. Infra changes — `sp-hq-infra/apt-deployment` (now a submodule of `hq`)

Canonical home for these edits is the `sp-hq-infra` submodule, NOT loose copies in cams.
(The earlier `cams/_package/apt-infra/` copies were removed once the submodule synced.)

### 2a. `regenerate-release.sh` (stable) — DONE
- **Scoped the `main` scans from `pool/` → `pool/main`.** `dpkg-scanpackages` is recursive, so the
  old `pool/` would have pulled the new `pool/hqcam` into TSR's `main`. This is the one line that
  touches the TSR flow; safe because TSR stable debs are deployed to `pool/main`.
- Added the `hqcam` component block (`armhf armv6 arm64`) scanning `pool/hqcam` →
  `dists/stable/hqcam/binary-$arch/Packages(.gz)`.

The trailing `generate-release.sh > Release` + `gpg` signing is unchanged — it re-signs the whole
dist, which now includes `hqcam` (pending the generator change in 2c).

### 2b. `regenerate-test-release.sh` (testing) — DONE
- `main` testing scans left as `pool/testing` (unchanged — no TSR disruption).
- Added the `hqcam` block scanning the **top-level** `pool/hqcam-testing` → `dists/testing/hqcam/…`.
  Top-level (not `pool/testing/hqcam`) so the recursive `pool/testing` scan can't catch it — no leak
  into TSR's `testing/main`.

> The synced submodule already fixed the old armhf-vs-other-arch `pool/testing` inconsistency, so no
> further fix needed there.

### 2c. Server-side generators (NOT in this repo) — STILL BLOCKING
There are **two**, both at `/home/tsradmin/sodapop/…`, neither synced into `apt-deployment`:
- `generate-release.sh`      (called by `regenerate-release.sh:32`)
- `generate-test-release.sh` (called by `regenerate-test-release.sh:41`)

Backwards-compatible change each needs:
- `Components:` line → `main hqcam` (additive; `main` stays, TSR untouched).
- Ensure the per-component/arch hashing also covers `hqcam/binary-*/Packages(.gz)`. If they
  auto-discover components by listing the dist's subdirs, the `hqcam` dirs are picked up and only
  `Components:` needs touching.

**Action:** copy both generators into `sp-hq-infra/apt-deployment/` and commit, then I'll make the
edits there. (`cleanup-apt-repo.sh`'s apt-ftparchive regen already auto-discovers components, so the
prune path needs no change.)

### 2d. `.path` watcher
`apt-release.path` watches the pool and triggers regeneration. Confirm it fires on changes under the
new `pool/hqcam` / `pool/hqcam-testing` paths (if it watches `pool/` recursively, no change needed).

---

## 3. cams-side packaging — `~/dev/hq/cams/_package`

(Separate from this plan's scope but required for an end-to-end deploy.)

- Fill in `_package/hqcam/DEBIAN/control` (Package: `hqcam`, Depends: `rpicam-apps`
  (older Pi OS: `libcamera-apps`), Architecture set per build), plus `postinst`/`preinst`
  (enable + start the `hqcam` service), adapted from `tsr/receiver/_package/tsr/DEBIAN/*`.
- Copy `_scripts/hqcam.service` into `_package/hqcam/etc/systemd/system/`.
- Port the build scripts from `tsr/receiver/_package/`: `build-deb.sh`, `copy-supporting-files.sh`,
  `_release.sh` (trim to Pi arches; drop the amd64 path), `repackage-without-zstd.sh`.
- Stage the cross-built `hqcam` binary into `_package/hqcam/usr/local/bin/` via the zigbuild matrix.

### 3a. New `_package/deploy.sh` (the pool-routing change)
Adapt `tsr/receiver/_package/deploy.sh` to upload into the **hqcam** pool instead of `pool/$repo`:

```bash
if [[ "$RC" == "true" ]]; then
  dest="pool/hqcam"            # stable
else
  dest="pool/hqcam-testing"   # testing  (match the convention chosen in 2b)
fi
...
cd /home/tsradmin/sodapop/apt-repo/$dest
put $latest_deb_file
```

---

## 4. Client-side — dedicated `add-sp-hqcam-repo.sh` (backwards-compatible)

**Decision (revised for backwards compatibility): leave the TSR `add-sp-apt-repo.sh` ×4 + trixie
variant UNTOUCHED.** Modifying the shared script to `stable main hqcam` would push the `hqcam`
component onto every TSR box's `sp.list`, and their `apt update` would error until `hqcam` exists in
the signed `Release`. Instead the camera Pis get their own script writing a separate sources file.

DONE — `~/dev/hq/cams/_tooling/add-sp-hqcam-repo.sh`: writes `/etc/apt/sources.list.d/sp-hqcam.list`
with `deb … stable hqcam` (its own file, never touches TSR's `sp.list`). Cameras only need the
`hqcam` component — `rpicam-apps` comes from Pi OS's own repos.

Uses the **modern keyring flow** (not `apt-key`, which is removed on Bookworm/trixie): the repo
public key is dearmored into `/etc/apt/keyrings/trusted.gpg` and referenced via `signed-by=` — the
same shared repo key as TSR. No new/per-pool key is needed; the dist `Release` signature covers the
`hqcam` component.

---

## 5. Bring-up / verification order

1. Server: create `pool/hqcam/` (+ testing pool), edit `generate-release.sh` `Components:` (2c).
2. Infra repo: land the `regenerate-release.sh` / `regenerate-test-release.sh` blocks (2a/2b), redeploy via `install-apt-release.sh` if the unit body changed.
3. cams: finish `control`/scripts, cross-build, produce a `.deb`, `deploy.sh` to `pool/hqcam-testing` first.
4. Confirm regen ran: `dists/testing/hqcam/binary-arm64/Packages` lists `hqcam`, and `InRelease` `Components:` includes `hqcam`.
5. On a camera Pi: run updated `add-sp-apt-repo.sh`, `sudo apt update`, `sudo apt install hqcam`, verify the `hqcam` service starts and streams on `:6877`.
6. Promote: deploy to `pool/hqcam` (stable), re-verify.

## Touch list
- `~/dev/sp-hq-infra/apt-deployment/regenerate-release.sh`
- `~/dev/sp-hq-infra/apt-deployment/regenerate-test-release.sh`
- server: `/home/tsradmin/sodapop/generate-release.sh` *(not in repo)*
- server: create `pool/hqcam/`, `pool/hqcam-testing/`
- `~/dev/hq/cams/_package/deploy.sh` *(new)* + control/scripts/build matrix
- `add-sp-apt-repo.sh` ×4 + trixie variant
