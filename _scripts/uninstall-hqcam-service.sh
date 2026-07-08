#!/usr/bin/env bash
set -e

SERVICE_NAME="${1:-hqcam}"

echo "Removing $SERVICE_NAME"

sudo systemctl stop "$SERVICE_NAME"
sudo systemctl disable "$SERVICE_NAME"

sudo rm -f "/etc/systemd/system/$SERVICE_NAME.service"

sudo systemctl daemon-reload
sudo systemctl reset-failed

echo "$SERVICE_NAME removed"
