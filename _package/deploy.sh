#!/bin/bash
# Ported from ~/dev/tsr/receiver/_package/deploy.sh. Uploads the latest-built deb
# into the dedicated hqcam pool (NOT tsr's pool/main) so the camera packages stay
# segregated from the TSR receivers.
APT_SERVER_IP=$1
RECENCY=$2
RC=$3

if [[ "$RC" == "true" ]]; then
  repo="hqcam"          # stable
else
  repo="hqcam-testing"  # testing (top-level pool, not under pool/testing)
fi
latest_deb_file=$(ls -t deb/*.deb | head -$RECENCY | tail -n 1)
echo "Deploying $latest_deb_file to $APT_SERVER_IP @ pool $repo"
lftp -u tsradmin,Zoipdrazz324? sftp://tsradmin@$APT_SERVER_IP <<EOF
cd /home/tsradmin/sodapop/apt-repo/pool/$repo
put $latest_deb_file
exit
EOF
