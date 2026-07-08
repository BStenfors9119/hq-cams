#!/usr/bin/env bash
# Release hqcam. Ported from ~/dev/tsr/receiver/_package/_release.sh: bump the
# version, cross-compile the arch × glibc matrix with cargo zigbuild, package
# each arch into a .deb, and optionally deploy to the apt server.
#
# Dropped vs. tsr: the armhf openssl + libatomic-shim setup (those exist for
# aws-lc-rs, which hqcam does not depend on — it's pure Rust).
#
# Usage:
#   ./_release.sh <major|minor|patch> <deploy true|false> <amd64|armhf|all> <rc true|false> [APT_SERVER_IP]
BUMP_TYPE=$1
DEPLOY=$2
BUILD_ARCH=$3
RC=$4
APT_SERVER_IP=$5

cd _package

if [ ! -d ./deb ]; then
  mkdir ./deb
fi

# The compiled binaries land here; the dir is empty (build artifacts only) so it
# isn't tracked in git. Create it or the `cp` below silently fails and the deb
# ships with no hqcam binary.
if [ ! -d ./hqcam/usr/local/bin ]; then
  mkdir -p ./hqcam/usr/local/bin
fi

bash ./copy-supporting-files.sh $RC

CONTROL_FILE="hqcam/DEBIAN/control"
## arch list. TSR ships arm64+armhf; cameras ship amd64+armhf. Add arm64
## (aarch64-unknown-linux-gnu) or armv6 here + in the triple case below.
declare -a archs=( "amd64" "armhf" )
## Pi OS glibc versions: Stretch 2.24, Buster 2.28, Bullseye 2.31, Bookworm 2.36.
declare -a ldds=( "2.24" "2.28" "2.31" "2.36" "2.41" )

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
echo "New version: $NEW_VERSION"
sed -i "s/^Version: .*/Version: $NEW_VERSION/" "$CONTROL_FILE"

for arch in "${archs[@]}"; do
  # Clear last arch's binaries so each arch's deb only carries its own (tsr
  # leaves these to the postinst rm; clearing here keeps the deb arch-clean).
  # rm -f hqcam/usr/local/bin/hqcam-*

  for ldd in "${ldds[@]}"; do
    sed -i "s/^Architecture: .*/Architecture: $arch/" "$CONTROL_FILE"

    if [[ "$arch" == "amd64" && ( "$BUILD_ARCH" == "amd64" || "$BUILD_ARCH" == "all" ) ]]; then
      TRIPLE=x86_64-unknown-linux-gnu
    elif [[ "$arch" == "armhf" && ( "$BUILD_ARCH" == "armhf" || "$BUILD_ARCH" == "all" ) ]]; then
      TRIPLE=arm-unknown-linux-gnueabihf
    else
      continue
    fi

    echo "Building $arch ($TRIPLE) glibc $ldd"
    cargo zigbuild \
      --manifest-path ../Cargo.toml \
      --target ${TRIPLE}.${ldd} \
      --target-dir target/${TRIPLE}.${ldd} \
      --release

    cp target/${TRIPLE}.${ldd}/${TRIPLE}/release/hqcam \
       hqcam/usr/local/bin/hqcam-$arch-${ldd}
  done

  echo "Packaging hqcam for $arch"
  bash ./build-deb.sh hqcam $arch
  if [ "$DEPLOY" == "true" ]; then
    echo "Deploying to $APT_SERVER_IP"
    bash ./deploy.sh $APT_SERVER_IP n1 $RC
  fi
done
