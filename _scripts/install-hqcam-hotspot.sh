#!/usr/bin/env bash
set -e

### Install the hqcam WiFi fallback-hotspot on the Raspberry Pi.
### Run as root from _scripts/. Requires NetworkManager (Pi OS Bookworm/Trixie).

### Toggle script
install -m 755 hqcam-hotspot.sh /usr/local/bin/hqcam-hotspot

### Config — only on first install, so an existing SSID/password is preserved.
mkdir -p /usr/local/etc/hqcam
if [ ! -f /usr/local/etc/hqcam/hotspot.conf ]; then
    cp hotspot.conf /usr/local/etc/hqcam/hotspot.conf
    echo "Installed default hotspot.conf — edit AP_SSID/AP_PASSWORD before use."
fi

### Systemd unit + timer
cp hqcam-hotspot.service hqcam-hotspot.timer /etc/systemd/system/
chmod 644 /etc/systemd/system/hqcam-hotspot.service /etc/systemd/system/hqcam-hotspot.timer

### Reload and enable the timer (which drives the decision service)
systemctl daemon-reload
systemctl enable --now hqcam-hotspot.timer

echo "Done. Check state with: hqcam-hotspot status"
