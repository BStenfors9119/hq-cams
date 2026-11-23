#!/usr/bin/env bash
# Point this camera's apt at the SodaPop hqcam repo.
#
#   add-sp-hqcam-repo.sh            # stable only (the default, every camera)
#   add-sp-hqcam-repo.sh testing    # stable AND testing — a test box
#
# On a test box both suites are listed and apt simply installs the highest
# version it can see: a testing build when one is newer than stable, and stable
# again once a release overtakes it. No pinning needed. Re-run with no argument
# to take a box back to stable only (it keeps whatever version is installed
# until a newer stable one arrives).
#
# Releases land in a suite via _package/deploy.sh: rc=true -> pool/hqcam
# (stable), rc=false -> pool/hqcam-testing (testing).
set -euo pipefail

CHANNEL=${1:-stable}
case "$CHANNEL" in
  stable)  SUITES=(stable) ;;
  testing) SUITES=(stable testing) ;;
  *) echo "usage: $0 [stable|testing]" >&2; exit 2 ;;
esac

ARCH=$(dpkg --print-architecture)

VPN_APT_BASE=http://10.24.3.46:1063/apt-repo
PUBLIC_APT_BASE=https://apt.sodapopsystems.com/apt-repo

# Camera-only repo file, kept separate from TSR's sp.list so the receivers' apt
# flow is never touched. Cameras only need the `hqcam` component; rpicam-apps
# comes from Pi OS's own repos.
#
# Two mirrors in priority order: the VPN/LAN host (fast, at HQ) then the public
# HTTPS mirror (fallback in the field).
#
# Modern keyring flow: apt-key is removed on Bookworm/trixie, so dearmor the
# repo public key into /etc/apt/keyrings and reference it via signed-by=
# (same key + keyring path as the TSR trixie script — one shared repo key).
sudo mkdir -p /etc/apt/keyrings
echo "Acquire::By-Hash=yes;" | sudo tee /etc/apt/apt.conf.d/99acquire-by-hash

LIST=/etc/apt/sources.list.d/sp-hqcam.list
{
  echo "# hqcam channel: ${CHANNEL} (written by add-sp-hqcam-repo.sh)"
  for suite in "${SUITES[@]}"; do
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${VPN_APT_BASE} ${suite} hqcam"
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${PUBLIC_APT_BASE} ${suite} hqcam"
  done
} | sudo tee "$LIST"

wget --no-check-certificate -vO - ${VPN_APT_BASE}/pubkey.gpg | sudo gpg --batch --yes --dearmor -o /etc/apt/keyrings/trusted.gpg

sudo apt-get update -o Dir::Etc::sourcelist=/dev/null -o Dir::Etc::sourceparts=/etc/apt/sources.list.d
apt-cache policy hqcam | head -3
