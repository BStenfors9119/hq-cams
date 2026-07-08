#!/usr/bin/env bash
# Ported from ~/dev/tsr/receiver/_package/build-deb.sh. Builds the staged hqcam
# tree into a .deb, names it by version+arch, then repackages it without zstd
# (older apt on Raspberry Pi OS can't read zstd debs). Called by _release.sh.
DEB_PKG=$1
ARCH=$2

echo $DEB_PKG

CONTROL_FILE="hqcam/DEBIAN/control"

VER=$(grep 'Version:' $CONTROL_FILE | awk -F":" '{print$2}' | tr -d '[:space:]')

echo $VER
echo ${DEB_PKG}_${VER}_${ARCH}
dpkg --build $DEB_PKG

mv ${DEB_PKG}.deb ${DEB_PKG}_${VER}_${ARCH}.deb
bash ./repackage-without-zstd.sh ${DEB_PKG}_${VER}_${ARCH}
