#!/usr/bin/env bash
set -e

### Install the hqcam footage-sync on the Raspberry Pi.
### Run as root from _scripts/. Requires rsync + openssh-client.

### Sync script
install -m 755 hqcam-sync.sh /usr/local/bin/hqcam-sync

### Config — only on first install, so an existing dest/settings are preserved.
mkdir -p /usr/local/etc/hqcam
if [ ! -f /usr/local/etc/hqcam/sync.conf ]; then
    cp sync.conf /usr/local/etc/hqcam/sync.conf
    echo "Installed default sync.conf — set SYNC_ENABLED=yes and SYNC_DEST before use."
fi

### Systemd unit + timer
cp hqcam-sync.service hqcam-sync.timer /etc/systemd/system/
chmod 644 /etc/systemd/system/hqcam-sync.service /etc/systemd/system/hqcam-sync.timer

### Reload and enable the timer (which drives the sync service)
systemctl daemon-reload
systemctl enable --now hqcam-sync.timer

echo "Done. Check state with: hqcam-sync status"
