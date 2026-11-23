#!/bin/bash
# Ported from ~/dev/tsr/receiver/_package/copy-supporting-files.sh. hqcam ships
# far less than tsr — just the systemd unit and the repo-add helper — so this is
# the same shape with hqcam's (short) file lists.
RC=$1

# Any missing source file must fail the build — otherwise the deb ships without
# its units/scripts and still "succeeds".
set -e

# Repo root = this script's parent dir, so it works from any clone (the sp-builder
# on Frasier clones to its own work dir; a hardcoded ~/dev/hq/cams doesn't exist there).
cd "$(dirname "$0")/.."

PKG_SUPPORTING_TOOLING_FILES_PATH=_package/hqcam/usr/local/etc/hqcam/_tooling
PKG_SUPPORTING_SYSTEMD_FILES_PATH=_package/hqcam/etc/systemd/system
PKG_SUPPORTING_BIN_FILES_PATH=_package/hqcam/usr/local/bin
PKG_SUPPORTING_ETC_FILES_PATH=_package/hqcam/usr/local/etc/hqcam
PKG_SUPPORTING_NM_FILES_PATH=_package/hqcam/etc/NetworkManager/conf.d

HQCAM_TOOLING=( "add-sp-hqcam-repo.sh" )
# hqcam.service + the WiFi fallback-hotspot unit and timer + the USB
# footage-drive mount unit for the offline recorder + the footage-sync unit/timer.
HQCAM_SYSTEMD_FILES=( "hqcam.service" "hqcam-hotspot.service" "hqcam-hotspot.timer" "media-hqcam.mount" "hqcam-sync.service" "hqcam-sync.timer" )

for tool in "${HQCAM_TOOLING[@]}"; do
    if [ ! -d $PKG_SUPPORTING_TOOLING_FILES_PATH ]; then
        mkdir -p $PKG_SUPPORTING_TOOLING_FILES_PATH
    fi
    cp ./_tooling/${tool} $PKG_SUPPORTING_TOOLING_FILES_PATH/${tool}
done

for systemd in "${HQCAM_SYSTEMD_FILES[@]}"; do
    if [ ! -d $PKG_SUPPORTING_SYSTEMD_FILES_PATH ]; then
        mkdir -p $PKG_SUPPORTING_SYSTEMD_FILES_PATH
    fi
    cp ./_scripts/$systemd $PKG_SUPPORTING_SYSTEMD_FILES_PATH/$systemd
done

# WiFi fallback-hotspot toggle script → /usr/local/bin/hqcam-hotspot.
mkdir -p $PKG_SUPPORTING_BIN_FILES_PATH
cp ./_scripts/hqcam-hotspot.sh $PKG_SUPPORTING_BIN_FILES_PATH/hqcam-hotspot

# Footage-sync script → /usr/local/bin/hqcam-sync.
cp ./_scripts/hqcam-sync.sh $PKG_SUPPORTING_BIN_FILES_PATH/hqcam-sync

# 5 GHz USB dongle diagnostic → /usr/local/bin/hqcam-verify-dongle. Read-only;
# run on the Pi to check a dongle's chipset/driver/5 GHz/AP-mode support.
cp ./_scripts/verify-dongle.sh $PKG_SUPPORTING_BIN_FILES_PATH/hqcam-verify-dongle

# Dual-radio setup → /usr/local/bin/hqcam-dual-radio. Run on the Pi to use a
# 5 GHz USB dongle for the HQ client link while keeping the onboard radio as the
# fallback-AP host (pins the dongle name, binds hqcam-wifi to it, edits confs).
cp ./_scripts/hqcam-dual-radio.sh $PKG_SUPPORTING_BIN_FILES_PATH/hqcam-dual-radio

# Hotspot config ships as a .default; postinst copies it into place only when no
# hotspot.conf exists yet, so a device's edited SSID/password survives upgrades.
mkdir -p $PKG_SUPPORTING_ETC_FILES_PATH
cp ./_scripts/hotspot.conf $PKG_SUPPORTING_ETC_FILES_PATH/hotspot.conf.default

# Offline-recorder config ships the same way (record.conf.default -> installed
# once as record.conf), so a device's recorder tuning survives upgrades.
cp ./_scripts/record.conf $PKG_SUPPORTING_ETC_FILES_PATH/record.conf.default

# Footage-sync config, same first-install-only pattern (sync.conf.default).
cp ./_scripts/sync.conf $PKG_SUPPORTING_ETC_FILES_PATH/sync.conf.default

# NetworkManager drop-in that disables WiFi power-save for a more reliable link
# at range. Ships to /etc/NetworkManager/conf.d/ (managed file, not user config).
mkdir -p $PKG_SUPPORTING_NM_FILES_PATH
cp ./_scripts/wifi-powersave-off.conf $PKG_SUPPORTING_NM_FILES_PATH/wifi-powersave-off.conf
