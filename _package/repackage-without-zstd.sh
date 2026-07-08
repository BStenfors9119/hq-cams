#!/usr/bin/env bash
# Ported from ~/dev/tsr/receiver/_package/repackage-without-zstd.sh. dpkg --build
# defaults to zstd-compressed members on modern dpkg; older apt on Raspberry Pi
# OS can't read those, so re-pack the .deb with xz members instead.

PACKAGE_NAME=$1
echo "Repackaging $PACKAGE_NAME"
echo "Extracting package....$PACKAGE_NAME.deb"
ar x ./$PACKAGE_NAME.deb
zstd -d < control.tar.zst | xz > control.tar.xz
zstd -d < data.tar.zst | xz > data.tar.xz
echo "Repackaging....."
ar -m -c -a sdsd ${PACKAGE_NAME}_repackaged.deb debian-binary control.tar.xz data.tar.xz
echo "Copying repackaged package....${PACKAGE_NAME}_repackaged.deb"
cp ./${PACKAGE_NAME}_repackaged.deb deb/$PACKAGE_NAME.deb
echo "Cleaning up...."
rm ./${PACKAGE_NAME}_repackaged.deb
rm ./${PACKAGE_NAME}.deb
rm ./control.tar.zst
rm ./data.tar.zst
rm ./control.tar.xz
rm ./data.tar.xz
rm ./debian-binary
ls
