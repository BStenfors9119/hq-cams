#!/usr/bin/env bash
# Ported from ~/dev/tsr/receiver/_package/repackage-without-zstd.sh. Older dpkg on
# the Pis can't read zstd-compressed debs. Ubuntu/Debian/Fedora-workstation
# dpkg-deb may write zstd members, so those get recompressed to xz; the sp-builder
# container on Frasier already writes xz, so that deb is used as-is. Assuming zstd
# unconditionally produced 256-byte empty debs that still exited 0 (hqcam 0.1.26,
# 2026-11-22 — same bug the receiver hit earlier).
set -euo pipefail

PACKAGE_NAME=$1
echo "Repackaging $PACKAGE_NAME"
echo "Extracting package....$PACKAGE_NAME.deb"
ar x ./$PACKAGE_NAME.deb

if [ -f control.tar.zst ] || [ -f data.tar.zst ]; then
  for member in control data; do
    if [ -f $member.tar.zst ]; then
      zstd -d < $member.tar.zst | xz > $member.tar.xz
    fi
  done
  echo "Repackaging....."
  ar -m -c -a sdsd ${PACKAGE_NAME}_repackaged.deb debian-binary control.tar.xz data.tar.xz
  echo "Copying repackaged package....${PACKAGE_NAME}_repackaged.deb"
  cp ./${PACKAGE_NAME}_repackaged.deb deb/$PACKAGE_NAME.deb
else
  echo "Already xz — no repackaging needed"
  cp ./$PACKAGE_NAME.deb deb/$PACKAGE_NAME.deb
fi

echo "Cleaning up...."
rm -f ./${PACKAGE_NAME}_repackaged.deb ./${PACKAGE_NAME}.deb debian-binary \
  control.tar.zst data.tar.zst control.tar.xz data.tar.xz

# Sanity check: the deb must open and carry exactly ONE capture binary, staged as
# usr/local/bin/hqcam-<arch> for this package's arch (hqcam_<ver>_<arch>). The
# sibling tools (hqcam-hotspot, hqcam-sync, ...) are excluded by matching the arch
# names explicitly. Listing goes to a variable first — piping into `grep -q`
# SIGPIPEs dpkg-deb and pipefail reads it as failure.
DEB_ARCH="${PACKAGE_NAME##*_}"
listing=$(dpkg-deb -c deb/$PACKAGE_NAME.deb)
binaries=$(grep -oE 'usr/local/bin/hqcam-(arm64|armhf)$' <<< "$listing" || true)
if [ "$binaries" != "usr/local/bin/hqcam-$DEB_ARCH" ]; then
  echo "deb/$PACKAGE_NAME.deb must contain exactly usr/local/bin/hqcam-$DEB_ARCH; found:"
  printf '  %s\n' ${binaries:-<none>}
  exit 1
fi
if ! grep -q 'etc/systemd/system/hqcam.service$' <<< "$listing"; then
  echo "deb/$PACKAGE_NAME.deb is broken — no hqcam.service inside"
  exit 1
fi
ls -l deb/$PACKAGE_NAME.deb
