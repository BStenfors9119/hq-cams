#!/usr/bin/env bash
#
# hqcam-dual-radio.sh — configure an hqcam Pi to use a 5 GHz USB dongle for the
# HQ client link while keeping the onboard radio as the fallback-AP host.
#
# Run ON THE PI (as root, or it re-execs with sudo). It:
#   1. Auto-detects the dongle (a wlan* whose driver isn't brcmfmac and that
#      advertises 5 GHz) and the onboard radio (brcmfmac).
#   2. Pins the dongle to a stable name (default wlan5g) via a udev rule keyed to
#      its MAC, so USB enumeration order can't swap it with the onboard radio.
#   3. Binds the HQ client connection (hqcam-wifi) to the dongle BY MAC — robust
#      to any renaming.
#   4. Leaves the fallback AP on the onboard radio (hotspot.conf AP_IFACE) and
#      points the recorder's online-check at the dongle (record.conf).
#   5. Reboots so the rename + bindings take effect.
#
# Why keep the onboard alive instead of `dtoverlay=disable-wifi`: Realtek dongles
# use out-of-tree DKMS drivers that a kernel update can break. Keeping the onboard
# radio as the AP host means that even if the dongle driver dies you can still
# reach the cam over its hqcam AP to fix it. You also get client + AP at once.
#
# Idempotent: safe to re-run. Use --dry-run to preview, --no-reboot to stop short.
#
# Usage: sudo ./hqcam-dual-radio.sh [options]
#   --dongle-mac MAC      use this dongle MAC instead of auto-detecting
#   --dongle-iface IFC    use this dongle interface instead of auto-detecting
#   --onboard-iface IFC   onboard radio name for the AP (default: the brcmfmac one)
#   --client-con NAME     NM client connection to bind (default: hqcam-wifi)
#   --stable-name NAME    stable name to give the dongle (default: wlan5g)
#   --dry-run             print what would change; make no changes
#   --no-reboot           apply changes but don't reboot
#   --yes                 skip the confirmation prompt
#   -h, --help            this help

set -uo pipefail

# ---- defaults ---------------------------------------------------------------
DONGLE_MAC=""
DONGLE_IFACE=""
ONBOARD_IFACE=""
CLIENT_CON="hqcam-wifi"
STABLE_NAME="wlan5g"
DRY=0
NO_REBOOT=0
ASSUME_YES=0

UDEV_RULE="/etc/udev/rules.d/72-hqcam-wifi.rules"
RECORD_CONF="/usr/local/etc/hqcam/record.conf"
HOTSPOT_CONF="/usr/local/etc/hqcam/hotspot.conf"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32m✓ %s\033[0m\n' "$*"; }
warn() { printf '  \033[33m! %s\033[0m\n' "$*"; }
fail() { printf '  \033[31m✗ %s\033[0m\n' "$*" >&2; }
die()  { fail "$*"; exit 1; }

# ---- args -------------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --dongle-mac)    DONGLE_MAC="$2"; shift 2;;
    --dongle-iface)  DONGLE_IFACE="$2"; shift 2;;
    --onboard-iface) ONBOARD_IFACE="$2"; shift 2;;
    --client-con)    CLIENT_CON="$2"; shift 2;;
    --stable-name)   STABLE_NAME="$2"; shift 2;;
    --dry-run)       DRY=1; shift;;
    --no-reboot)     NO_REBOOT=1; shift;;
    --yes)           ASSUME_YES=1; shift;;
    -h|--help)       sed -n '2,40p' "$0"; exit 0;;
    *) die "unknown argument: $1 (try --help)";;
  esac
done

# ---- must be root (re-exec via sudo if not) ---------------------------------
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null || die "run as root (sudo not available)"
  exec sudo "$0" \
    ${DONGLE_MAC:+--dongle-mac "$DONGLE_MAC"} \
    ${DONGLE_IFACE:+--dongle-iface "$DONGLE_IFACE"} \
    ${ONBOARD_IFACE:+--onboard-iface "$ONBOARD_IFACE"} \
    --client-con "$CLIENT_CON" --stable-name "$STABLE_NAME" \
    $([ "$DRY" = 1 ] && echo --dry-run) \
    $([ "$NO_REBOOT" = 1 ] && echo --no-reboot) \
    $([ "$ASSUME_YES" = 1 ] && echo --yes)
fi

command -v iw    >/dev/null || die "iw missing — sudo apt install -y iw"
command -v nmcli >/dev/null || die "nmcli missing — NetworkManager is required"

# run <cmd...> — execute, or just print under --dry-run.
run() {
  if [ "$DRY" = 1 ]; then printf '  [dry-run] %s\n' "$*"; else "$@"; fi
}

driver_of() { basename "$(readlink -f "/sys/class/net/$1/device/driver" 2>/dev/null)" 2>/dev/null; }
mac_of()    { cat "/sys/class/net/$1/address" 2>/dev/null; }
phy_of()    { cat "/sys/class/net/$1/phy80211/name" 2>/dev/null; }
is5ghz()    { local p; p="$(phy_of "$1")"; { iw phy "$p" channels 2>/dev/null; iw phy "$p" info 2>/dev/null; } | grep -qE '5[0-9]{3} MHz'; }

bold "== hqcam dual-radio setup =="
[ "$DRY" = 1 ] && warn "DRY RUN — no changes will be made"

# ---- detect the radios ------------------------------------------------------
for net in /sys/class/net/wlan*; do
  [ -e "$net" ] || continue
  ifc="$(basename "$net")"; drv="$(driver_of "$ifc")"
  if [ "$drv" = brcmfmac ]; then
    [ -n "$ONBOARD_IFACE" ] || ONBOARD_IFACE="$ifc"
  else
    # Prefer a dongle that actually does 5 GHz.
    if [ -z "$DONGLE_IFACE" ] && is5ghz "$ifc"; then DONGLE_IFACE="$ifc"; fi
  fi
done
# Fall back to any non-onboard wlan if none advertised 5 GHz (e.g. radio down).
if [ -z "$DONGLE_IFACE" ]; then
  for net in /sys/class/net/wlan*; do
    [ -e "$net" ] || continue
    ifc="$(basename "$net")"
    [ "$(driver_of "$ifc")" = brcmfmac ] && continue
    DONGLE_IFACE="$ifc"; break
  done
fi

[ -n "$DONGLE_IFACE" ] || die "no USB WiFi dongle found (only the onboard radio). Plug it in and re-run."
[ -n "$DONGLE_MAC" ] || DONGLE_MAC="$(mac_of "$DONGLE_IFACE")"
[ -n "$DONGLE_MAC" ] || die "could not read the dongle MAC for $DONGLE_IFACE"
DONGLE_MAC="$(printf '%s' "$DONGLE_MAC" | tr 'A-Z' 'a-z')"
[ -n "$ONBOARD_IFACE" ] || { ONBOARD_IFACE="wlan0"; warn "no onboard (brcmfmac) radio detected; assuming AP host = wlan0"; }

is5ghz "$DONGLE_IFACE" && pass "dongle $DONGLE_IFACE does 5 GHz" \
  || warn "dongle $DONGLE_IFACE didn't advertise 5 GHz right now (radio down?) — proceeding"

# ---- resolve the client connection ------------------------------------------
con_exists() { nmcli -t -f NAME con show 2>/dev/null | grep -qxF "$1"; }
if ! con_exists "$CLIENT_CON"; then
  alt="$(nmcli -t -f NAME,TYPE con show 2>/dev/null | awk -F: '$2 ~ /wireless/ {print $1; exit}')"
  if [ -n "$alt" ]; then
    warn "client connection '$CLIENT_CON' not found; using WiFi connection '$alt' instead"
    CLIENT_CON="$alt"
  else
    warn "no WiFi client connection found; skipping the client bind (create one, then re-run)"
    CLIENT_CON=""
  fi
fi

# ---- plan -------------------------------------------------------------------
bold "Plan:"
echo "  dongle (5 GHz client) : $DONGLE_IFACE  [$DONGLE_MAC]  ->  rename to $STABLE_NAME"
echo "  onboard (AP fallback) : $ONBOARD_IFACE"
echo "  client connection     : ${CLIENT_CON:-<none — skipped>}  ->  bound to the dongle MAC"
echo "  udev rule             : $UDEV_RULE"
echo "  record.conf           : HQCAM_WIFI_IFACE=$STABLE_NAME"
echo "  hotspot.conf          : AP_IFACE=\"$ONBOARD_IFACE\""
echo
if [ "$DRY" != 1 ] && [ "$ASSUME_YES" != 1 ]; then
  read -rp "Apply this and reboot? [y/N] " ans
  case "$ans" in y|Y|yes|YES) ;; *) die "aborted";; esac
fi

# ---- 1. pin the dongle to a stable name via udev ----------------------------
bold "Pinning $DONGLE_IFACE -> $STABLE_NAME (udev, by MAC):"
RULE_LINE="SUBSYSTEM==\"net\", ACTION==\"add\", ATTR{address}==\"$DONGLE_MAC\", NAME=\"$STABLE_NAME\""
if [ "$DRY" = 1 ]; then
  printf '  [dry-run] write %s:\n            %s\n' "$UDEV_RULE" "$RULE_LINE"
else
  printf '# hqcam: pin the 5 GHz USB dongle to a stable name so it never swaps with\n# the onboard radio across boots. Generated by hqcam-dual-radio.sh.\n%s\n' \
    "$RULE_LINE" > "$UDEV_RULE" && pass "wrote $UDEV_RULE"
  run udevadm control --reload
fi

# ---- 2. bind the HQ client connection to the dongle (by MAC) ----------------
if [ -n "$CLIENT_CON" ]; then
  bold "Binding client connection '$CLIENT_CON' to the dongle:"
  # MAC bind is authoritative; clear any interface-name pin so it can't fight it.
  run nmcli con modify "$CLIENT_CON" 802-11-wireless.mac-address "$DONGLE_MAC" connection.interface-name ""
  run nmcli con modify "$CLIENT_CON" connection.autoconnect yes
  pass "bound '$CLIENT_CON' to $DONGLE_MAC"
fi

# ---- 3. recorder online-check watches the client radio ----------------------
set_kv() { # set_kv <file> <KEY> <value>  (replace commented/uncommented line, else append)
  local file="$1" key="$2" val="$3"
  [ -f "$file" ] || { warn "$file not found (is hqcam installed?); skipping $key"; return; }
  if [ "$DRY" = 1 ]; then printf '  [dry-run] set %s=%s in %s\n' "$key" "$val" "$file"; return; fi
  if grep -qE "^[[:space:]]*#?[[:space:]]*${key}=" "$file"; then
    sed -i -E "s|^[[:space:]]*#?[[:space:]]*${key}=.*|${key}=${val}|" "$file"
  else
    printf '\n%s=%s\n' "$key" "$val" >> "$file"
  fi
  pass "set $key=$val in $file"
}
bold "Pointing the recorder online-check at the dongle:"
set_kv "$RECORD_CONF" HQCAM_WIFI_IFACE "$STABLE_NAME"

# ---- 4. keep the fallback AP on the onboard radio ---------------------------
bold "Keeping the fallback AP on the onboard radio:"
set_kv "$HOTSPOT_CONF" AP_IFACE "\"$ONBOARD_IFACE\""

# ---- 5. DKMS sanity for the Realtek driver ----------------------------------
bold "DKMS driver check:"
if command -v dkms >/dev/null 2>&1; then
  if dkms status 2>/dev/null | grep -qiE '88x2bu|8822bu|8812bu|rtl88'; then
    pass "a Realtek DKMS module is registered — it should rebuild on kernel updates"
  else
    warn "no 88x2bu/rtl88 DKMS module registered; a kernel update may break the dongle."
    warn "Install the DKMS package (e.g. rtl88x2bu-dkms / morrownr 88x2bu) so it survives upgrades."
  fi
else
  warn "dkms not installed; can't confirm the dongle driver survives kernel updates."
fi

# ---- 6. reboot --------------------------------------------------------------
echo
if [ "$DRY" = 1 ]; then
  bold "Dry run complete — no changes made."
elif [ "$NO_REBOOT" = 1 ]; then
  bold "Done (no reboot). Reboot to apply: sudo reboot"
  echo "  After reboot, verify:  iw dev ; nmcli device status ; iw dev $STABLE_NAME link"
else
  bold "Rebooting to apply..."
  echo "  After reboot, verify:  iw dev ; nmcli device status ; iw dev $STABLE_NAME link"
  run reboot
fi
