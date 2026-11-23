#!/usr/bin/env bash
# Release hqcam. Ported from ~/dev/tsr/receiver/_package/_release.sh: set/bump the
# version, cross-compile one binary per arch with cargo zigbuild, package each
# arch into a .deb, and optionally deploy to the apt server.
#
# Dropped vs. tsr: the armhf openssl + libatomic-shim setup (those exist for
# aws-lc-rs, which hqcam does not depend on — it's pure Rust).
#
# Usage:
#   ./_release.sh <major|minor|patch|none|X.Y.Z> <deploy true|false> <arm64|armhf|all> <rc true|false> [APT_SERVER_IP]
#   bump: major|minor|patch bumps DEBIAN/control's version; none keeps it; an explicit
#   X.Y.Z sets it outright (sp-builder passes the version picked in the admin CI/CD page).
BUMP_TYPE=$1
DEPLOY=$2
BUILD_ARCH=$3
RC=$4
APT_SERVER_IP=$5

# Run from _package/ regardless of the caller's cwd (sp-builder runs
# `bash _package/_release.sh` from the clone root).
cd "$(dirname "$0")" || exit 1

if [ ! -d ./deb ]; then
  mkdir ./deb
fi

# The compiled binaries land here; the dir is build artifacts only (gitignored).
# Create it or the `cp` below fails.
if [ ! -d ./hqcam/usr/local/bin ]; then
  mkdir -p ./hqcam/usr/local/bin
fi

bash ./copy-supporting-files.sh $RC || exit 1

CONTROL_FILE="hqcam/DEBIAN/control"
## arch list. Cameras run on Pis only: arm64 (Zero 2 W / Pi 3+) + armhf (Zero W /
## original). Add armv6 (arm-unknown-linux-gnueabi) here + in the triple case below
## if needed.
declare -a archs=( "arm64" "armhf" )
## ONE glibc build per arch, same as tsr. glibc is backward compatible, so a binary
## linked against the 2.24 floor (Stretch) runs on Buster/Bullseye/Bookworm/Trixie
## too. The old 2.24/2.28/2.31/2.36/2.41 matrix only existed because postinst
## picked `hqcam-<arch>-<ldd>` by EXACT glibc match — 10 LTO builds per release.
GLIBC_FLOOR="2.24"

VERSION_LINE=$(grep '^Version:' "$CONTROL_FILE")
VERSION=$(echo "$VERSION_LINE" | awk '{print $2}')

# Validate semantic version format (major.minor.patch)
if ! echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "Error: Version '$VERSION' is not in semantic format (major.minor.patch)"
    exit 1
fi

# Split version into major, minor, patch
MAJOR=$(echo "$VERSION" | cut -d. -f1)
MINOR=$(echo "$VERSION" | cut -d. -f2)
PATCH=$(echo "$VERSION" | cut -d. -f3)

echo "MAJOR $MAJOR, MINOR $MINOR, PATCH $PATCH"
# Increment version based on bump type
case "$BUMP_TYPE" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
esac

NEW_VERSION="$MAJOR.$MINOR.$PATCH"
if echo "$BUMP_TYPE" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    NEW_VERSION="$BUMP_TYPE"
fi
echo "New version: $NEW_VERSION"
sed -i "s/^Version: .*/Version: $NEW_VERSION/" "$CONTROL_FILE"
# Stamp the same version into the crate, so the binary reports it
# (CARGO_PKG_VERSION -> /api/status "version"). Only the [package] version —
# the first `version =` line in Cargo.toml. Cargo updates Cargo.lock to match.
sed -i "0,/^version = /s/^version = .*/version = \"$NEW_VERSION\"/" ../Cargo.toml
grep -q "^version = \"$NEW_VERSION\"" ../Cargo.toml || { echo "Error: couldn't stamp $NEW_VERSION into Cargo.toml"; exit 1; }

for arch in "${archs[@]}"; do
  # Only arches requested this run are built and packaged.
  if [[ "$BUILD_ARCH" != "$arch" && "$BUILD_ARCH" != "all" ]]; then
    continue
  fi

  # Each arch's deb ships ONLY its own binary. usr/local/bin is gitignored, so old
  # binaries survive sp-builder's `git clean` and dpkg --build packs the whole tree:
  # clear the staged hqcam binaries (new `hqcam-<arch>` and legacy
  # `hqcam-<arch>-2.NN`) before every arch. Named explicitly so the sibling tools
  # (hqcam-hotspot, hqcam-sync, ...) staged by copy-supporting-files.sh stay.
  rm -f hqcam/usr/local/bin/hqcam-arm64 hqcam/usr/local/bin/hqcam-armhf \
        hqcam/usr/local/bin/hqcam-*-2.*

  sed -i "s/^Architecture: .*/Architecture: $arch/" "$CONTROL_FILE"

  if [[ "$arch" == "arm64" ]]; then
    TRIPLE=aarch64-unknown-linux-gnu
  else
    TRIPLE=arm-unknown-linux-gnueabihf
  fi

  echo "Building hqcam for $arch ($TRIPLE, glibc ${GLIBC_FLOOR} floor)"
  # Same target dir as the old 2.24 matrix entry, so existing build caches are reused.
  cargo zigbuild \
    --manifest-path ../Cargo.toml \
    --target ${TRIPLE}.${GLIBC_FLOOR} \
    --target-dir target/${TRIPLE}.${GLIBC_FLOOR} \
    --release || exit 1

  # Staged as hqcam-<arch>; postinst moves it to /usr/local/bin/hqcam.
  cp target/${TRIPLE}.${GLIBC_FLOOR}/${TRIPLE}/release/hqcam \
     hqcam/usr/local/bin/hqcam-$arch || exit 1

  echo "Packaging hqcam for $arch"
  bash ./build-deb.sh hqcam $arch || exit 1
  if [ "$DEPLOY" == "true" ]; then
    echo "Deploying to $APT_SERVER_IP"
    bash ./deploy.sh $APT_SERVER_IP n1 $RC || exit 1
  fi
done
