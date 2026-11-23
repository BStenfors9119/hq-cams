#!/usr/bin/env bash
# hqcam WiFi fallback hotspot.
#
# Join the HQ WiFi when it is in range (so Motion Eye keeps pulling the stream
# at home); otherwise broadcast our own access point so the cam is reachable in
# the vehicle at http://10.42.0.1:6877.
#
# With only the onboard radio a Pi is EITHER a client OR an AP — never both — and
# this script decides which, based on whether a known network is reachable. In a
# dual-radio setup (a 5 GHz USB dongle for the HQ client link + the onboard radio
# hosting the AP) both can run at once: client detection is radio-agnostic (any
# interface but our AP), while the AP always binds IFACE (the onboard radio).
#
# NetworkManager only (Raspberry Pi OS Bookworm / Trixie). NM's `ipv4.method
# shared` gives the AP its own DHCP + DNS on 10.42.0.0/24 (gateway 10.42.0.1) —
# no hostapd/dnsmasq to configure.
set -euo pipefail

CONF="${HQCAM_HOTSPOT_CONF:-/usr/local/etc/hqcam/hotspot.conf}"
# shellcheck source=/dev/null
[ -f "$CONF" ] && . "$CONF"

AP_SSID="${AP_SSID:-hqcam}"
AP_PASSWORD="${AP_PASSWORD:-hqcam1234}"
IFACE="${AP_IFACE:-wlan0}"
AP_CON="${AP_CON_NAME:-hqcam-ap}"
GRACE="${AP_GRACE:-90}"
REJOIN_EVERY="${AP_REJOIN_EVERY:-600}"
REJOIN_GRACE="${AP_REJOIN_GRACE:-20}"
REJOIN_SKIP_IF_CLIENTS="${AP_REJOIN_SKIP_IF_CLIENTS:-yes}"
STATE_DIR="${HQCAM_STATE_DIR:-/run/hqcam}"

log() { echo "hqcam-hotspot: $*"; }

# When the AP last (re)started or last attempted a rejoin — the clock for the
# next periodic rejoin. Stored under /run (tmpfs), so it resets on reboot.
stamp_rejoin() { mkdir -p "$STATE_DIR"; date +%s > "$STATE_DIR/last-rejoin"; }
secs_since_rejoin() {
  [ -f "$STATE_DIR/last-rejoin" ] || { echo 999999; return; }
  echo $(( $(date +%s) - $(cat "$STATE_DIR/last-rejoin") ))
}

# Is a station currently associated with our AP (someone viewing)? Best-effort:
# if `iw` is missing or errors, assume no clients so a rejoin can still proceed.
ap_has_clients() {
  command -v iw >/dev/null 2>&1 || return 1
  iw dev "$IFACE" station dump 2>/dev/null | grep -q "^Station"
}

# Name of an active CLIENT wifi connection — on ANY interface, excluding our own
# AP. Radio-agnostic on purpose: in a dual-radio setup (a 5 GHz USB dongle for
# the HQ link + the onboard radio hosting the AP) the client link is on a
# different interface (e.g. wlan5g) than IFACE. Pinning this to IFACE would read
# "offline" while the dongle is happily connected, and wrongly raise the AP.
client_wifi_name() {
  nmcli -t -f NAME,TYPE connection show --active \
    | awk -F: -v ap="$AP_CON" '$2=="802-11-wireless" && $1!=ap {print $1; exit}'
}

client_connected() { [ -n "$(client_wifi_name)" ]; }

# Is OUR AP connection active on the AP interface?
ap_active() {
  nmcli -t -f NAME,DEVICE,TYPE connection show --active \
    | awk -F: -v ap="$AP_CON" -v d="$IFACE" \
        '$3=="802-11-wireless" && $1==ap && $2==d {found=1} END{exit(found?0:1)}'
}

# Create the AP profile if missing, or refresh its SSID/password from config.
# Kept in sync every run so editing hotspot.conf + a restart is enough.
ensure_ap_profile() {
  if nmcli -t -f NAME connection show | grep -qx "$AP_CON"; then
    nmcli connection modify "$AP_CON" \
      802-11-wireless.ssid "$AP_SSID" \
      wifi-sec.key-mgmt wpa-psk \
      wifi-sec.psk "$AP_PASSWORD"
  else
    nmcli connection add type wifi ifname "$IFACE" con-name "$AP_CON" \
      autoconnect no ssid "$AP_SSID" \
      802-11-wireless.mode ap 802-11-wireless.band bg \
      ipv4.method shared \
      wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$AP_PASSWORD"
  fi
}

ap_up() {
  ensure_ap_profile
  log "starting AP '$AP_SSID' on $IFACE — cam at http://10.42.0.1:6877"
  nmcli connection up "$AP_CON"
  stamp_rejoin # restart the periodic-rejoin clock from now
}

# Briefly drop the AP and let NetworkManager try to autoconnect to HQ (or any
# known network). If it joins within REJOIN_GRACE we stay a client; otherwise we
# bring the AP back up. A single radio can't scan while running the AP, so this
# short interruption is the only way to notice HQ has come back into range.
attempt_rejoin() {
  log "AP up ${REJOIN_EVERY}s+ — dropping it briefly to look for HQ WiFi"
  nmcli connection down "$AP_CON" || true
  for _ in $(seq 1 "$REJOIN_GRACE"); do
    sleep 1
    if client_connected; then
      log "rejoined HQ WiFi '$(client_wifi_name)' — leaving AP down"
      return 0
    fi
  done
  log "no HQ WiFi found — restoring AP"
  ap_up
}

ap_down() {
  if ap_active; then
    log "stopping AP — NetworkManager will rejoin a known network if in range"
    nmcli connection down "$AP_CON"
  fi
}

# The decision run (invoked at boot and on the timer).
auto() {
  ensure_ap_profile

  if client_connected; then
    log "on WiFi '$(client_wifi_name)' — AP not needed"
    exit 0
  fi

  if ap_active; then
    # Periodically drop the AP to look for HQ WiFi so parking at HQ reconnects
    # on its own. Guarded: only when enabled, only once per REJOIN_EVERY, and
    # never while someone is actively viewing over the AP.
    if [ "$REJOIN_EVERY" -gt 0 ] && [ "$(secs_since_rejoin)" -ge "$REJOIN_EVERY" ]; then
      if [ "$REJOIN_SKIP_IF_CLIENTS" = "yes" ] && ap_has_clients; then
        log "AP has a connected viewer — skipping rejoin attempt this cycle"
        stamp_rejoin # reset the clock so we retry after another REJOIN_EVERY
      else
        attempt_rejoin
      fi
      exit 0
    fi
    log "AP already up — holding"
    exit 0
  fi

  # Disconnected and no AP: give NetworkManager a chance to autoconnect to a
  # known network before we fall back to broadcasting our own.
  log "no WiFi connection; waiting up to ${GRACE}s for a known network…"
  for _ in $(seq 1 "$GRACE"); do
    sleep 1
    if client_connected; then
      log "joined '$(client_wifi_name)'"
      exit 0
    fi
  done

  log "no known network found — falling back to AP"
  ap_up
}

case "${1:-auto}" in
  auto)   auto ;;
  on)     ap_up ;;
  off)    ap_down ;;
  status)
    if client_connected; then
      echo "client: $(client_wifi_name)"
    elif ap_active; then
      echo "ap: $AP_SSID (http://10.42.0.1:6877)"
    else
      echo "disconnected"
    fi ;;
  *) echo "usage: $0 {auto|on|off|status}" >&2; exit 2 ;;
esac
