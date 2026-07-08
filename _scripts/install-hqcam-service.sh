#!/usr/bin/env bash
set -e

### Install the hqcam service on the Raspberry Pi (run as root from _scripts/)
cp hqcam.service /etc/systemd/system/
chmod u+rxw /etc/systemd/system/hqcam.service

### Reload, enable on boot, and start now
systemctl daemon-reload
systemctl enable hqcam
systemctl start hqcam
