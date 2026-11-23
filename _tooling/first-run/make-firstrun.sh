#!/bin/bash
#
# Generate a concrete hqcam first-run script from firstrun.sh, with per-Pi values
# substituted in. Consumed by ../../../_scripts/new-hqcam-pi.sh, which bakes the
# output onto the boot partition and wires it to run once on first boot.
#
# Usage:
#   ./make-firstrun.sh --password <pw> [options]
#
# Options:
#   --password <pw>        (required) admin SSH password (hashed at generation
#                          time; NOT stored in plaintext)
#   --user <name>          admin username to create (default hqadmin)
#   --wifi-ssid <ssid>     bake HQ WiFi SSID (else read from boot hqcam-wifi.txt,
#                          else fall back to the on-board AP)
#   --wifi-psk <psk>       bake HQ WiFi password (pairs with --wifi-ssid)
#   --wifi-country <cc>    WiFi regulatory domain (default US)
#   --ap-ssid <ssid>       override the fallback-AP SSID (else package default)
#   --ap-password <pw>     override the fallback-AP passphrase (>=8 chars)
#   --sync-dest <dest>     enable footage-sync to this rsync-over-ssh dest
#                          ([user@]host:/path/) — also flips SYNC_ENABLED=yes
#   -o, --output <path>    output script path (default ./firstrun-hqcam.sh)
#
# WiFi/AP/sync all have safe fallbacks, so the only hard requirement is the admin
# password. A camera with no HQ WiFi baked still works via its fallback hotspot.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$SCRIPT_DIR/firstrun.sh"

PASSWORD=""
PASSWORD_SET=0
ADMIN_USER="hqadmin"
WIFI_SSID=""
WIFI_PSK=""
WIFI_COUNTRY=""
AP_SSID=""
AP_PASSWORD=""
SYNC_DEST=""
OUTPUT=""

die() { echo "ERROR: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --password)     PASSWORD="$2"; PASSWORD_SET=1; shift 2 ;;
        --user)         ADMIN_USER="$2"; shift 2 ;;
        --wifi-ssid)    WIFI_SSID="$2"; shift 2 ;;
        --wifi-psk)     WIFI_PSK="$2"; shift 2 ;;
        --wifi-country) WIFI_COUNTRY="$2"; shift 2 ;;
        --ap-ssid)      AP_SSID="$2"; shift 2 ;;
        --ap-password)  AP_PASSWORD="$2"; shift 2 ;;
        --sync-dest)    SYNC_DEST="$2"; shift 2 ;;
        -o|--output)    OUTPUT="$2"; shift 2 ;;
        -h|--help)      sed -n '2,29p' "$0"; exit 0 ;;
        *)              die "unknown argument: $1" ;;
    esac
done

[ -f "$TEMPLATE" ] || die "template not found at $TEMPLATE"
{ [ "$PASSWORD_SET" -eq 1 ] && [ -n "$PASSWORD" ]; } || die "--password is required (admin SSH password)"
[[ "$ADMIN_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || die "--user must be a valid unix username"
if [ -n "$AP_PASSWORD" ]; then
    [ "${#AP_PASSWORD}" -ge 8 ] || die "--ap-password must be at least 8 chars (WPA requirement)"
fi

OUTPUT="${OUTPUT:-$SCRIPT_DIR/firstrun-hqcam.sh}"

# Hash the admin password now (crypt SHA-512) so plaintext never lands on the SD.
command -v openssl >/dev/null 2>&1 || die "openssl is required to hash the password"
PWHASH="$(openssl passwd -6 "$PASSWORD")"

# Sync is enabled only when a destination is given.
SYNC_ENABLED=""
[ -n "$SYNC_DEST" ] && SYNC_ENABLED="yes"

# Escape a string for safe use as a sed replacement with '|' as the delimiter.
escape_repl() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }

sed \
    -e "s|__HQCAM_ADMIN_USER__|$(escape_repl "$ADMIN_USER")|g" \
    -e "s|__HQCAM_ADMIN_PWHASH__|$(escape_repl "$PWHASH")|g" \
    -e "s|__HQCAM_WIFI_SSID__|$(escape_repl "$WIFI_SSID")|g" \
    -e "s|__HQCAM_WIFI_PSK__|$(escape_repl "$WIFI_PSK")|g" \
    -e "s|__HQCAM_WIFI_COUNTRY__|$(escape_repl "$WIFI_COUNTRY")|g" \
    -e "s|__HQCAM_AP_SSID__|$(escape_repl "$AP_SSID")|g" \
    -e "s|__HQCAM_AP_PASSWORD__|$(escape_repl "$AP_PASSWORD")|g" \
    -e "s|__HQCAM_SYNC_DEST__|$(escape_repl "$SYNC_DEST")|g" \
    -e "s|__HQCAM_SYNC_ENABLED__|$(escape_repl "$SYNC_ENABLED")|g" \
    "$TEMPLATE" > "$OUTPUT"
chmod +x "$OUTPUT"

echo "Wrote $OUTPUT"
echo "  admin user  : $ADMIN_USER (hashed password; SSH password auth enabled)"
if [ -n "$WIFI_SSID" ]; then
    echo "  HQ wifi     : baked SSID '$WIFI_SSID'"
else
    echo "  HQ wifi     : not baked — drop hqcam-wifi.txt on boot, or use the fallback AP"
fi
[ -n "$AP_SSID" ]   && echo "  fallback AP : SSID '$AP_SSID'"      || echo "  fallback AP : package default (hqcam)"
[ -n "$SYNC_DEST" ] && echo "  footage sync: enabled -> $SYNC_DEST" || echo "  footage sync: disabled (no --sync-dest)"
