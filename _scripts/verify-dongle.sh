#!/usr/bin/env bash
#
# verify-dongle.sh — is this USB WiFi dongle a good 5 GHz radio for an hqcam Pi?
#
# Ported from ~/dev/tsr/prism/_load_test/tools/verify-dongle.sh, with an extra
# AP-MODE check that matters for cameras (see below).
#
# Run ON THE PI after plugging the dongle in. READ-ONLY: it identifies the
# chipset, the bound driver, whether the radio supports 5 GHz, and whether it can
# run as an access point. It makes NO config changes and won't touch your
# network — safe to run on a live camera or a bench Pi. Installed by the deb as
# `hqcam-verify-dongle`.
#
# Why 5 GHz for hqcam: a stronger, less-congested link to HQ so Motion Eye keeps
# pulling the stream and footage-sync completes at range. Why AP mode: the
# fallback hotspot (hqcam-hotspot) needs it. That gates which layout you use —
# the Verdict below spells it out.
#
# What "good" looks like: a mainline-driver chipset (Panda PAU09 = RT5572 /
# rt2800usb, or MediaTek MT7612U / mt76) on an interface that lists 5 GHz
# channels AND supports AP mode. Realtek dongles usually mean out-of-tree DKMS —
# flagged as a warning.
#
# Needs: usbutils (lsusb) and iw  →  sudo apt install -y usbutils iw
#        (the hqcam deb depends on both, so they're already present.)

set -uo pipefail

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32m✓ %s\033[0m\n' "$*"; }
warn() { printf '  \033[33m! %s\033[0m\n' "$*"; }
fail() { printf '  \033[31m✗ %s\033[0m\n' "$*"; }

bold "== hqcam dongle check =="

command -v lsusb >/dev/null || { fail "lsusb missing — sudo apt install usbutils"; exit 2; }
command -v iw    >/dev/null || { fail "iw missing — sudo apt install iw"; exit 2; }

# ── 1. Chipset by USB VID:PID ────────────────────────────────────────────────
bold "USB devices:"
lsusb | sed 's/^/  /'

usb="$(lsusb)"
chipset=""; driver_expect=""; verdict=""
if   grep -qi '148f:5572' <<<"$usb"; then
  chipset="Ralink/MediaTek RT5572 (Panda PAU09)"; driver_expect="rt2800usb"; verdict="good"
elif grep -qi '0e8d:7612' <<<"$usb"; then
  chipset="MediaTek MT7612U"; driver_expect="mt76|mt76x2u"; verdict="good"
elif grep -qi '0e8d:' <<<"$usb"; then
  chipset="MediaTek (generic — e.g. MT7921/6E)"; driver_expect="mt76 (verify)"; verdict="mtk"
elif grep -qiE '0bda:(8811|8812|8821|c811|b812)' <<<"$usb"; then
  chipset="Realtek (RTL88xx)"; driver_expect="rtw88|8812au(dkms)"; verdict="realtek"
elif grep -qiE '2357:' <<<"$usb"; then
  chipset="TP-Link (likely Realtek RTL8812AU/BU — e.g. Archer T3U/T4U AC1300)"
  driver_expect="88x2bu / 8812au (DKMS)"; verdict="realtek"
elif grep -qi '0bda:' <<<"$usb"; then
  chipset="Realtek (generic)"; driver_expect="rtw88/dkms"; verdict="realtek"
fi

bold "Chipset:"
if [ "$verdict" = good ]; then
  pass "$chipset — mainline driver ($driver_expect). Ideal for a headless fleet."
elif [ "$verdict" = mtk ]; then
  warn "$chipset — usually mainline mt76, but confirm THIS model's chip and that"
  warn "the driver survives 'sudo apt full-upgrade' + reboot before fleet use."
elif [ "$verdict" = realtek ]; then
  warn "$chipset — likely needs out-of-tree DKMS (kernel-update maintenance risk)."
  warn "Prefer a Panda PAU09 (148f:5572) or MediaTek MT7612U (0e8d:7612)."
else
  warn "No known hqcam-recommended WiFi chipset matched in lsusb."
  warn "If you just plugged it in, check the cable/adapter and re-run."
fi

# ── 2. Per-interface: driver, 5 GHz capability, AP mode ──────────────────────
bold "Wireless interfaces:"
found5=0; any_iface=0; dongle_iface=""; dongle_ap=0
for net in /sys/class/net/wlan*; do
  [ -e "$net" ] || continue
  any_iface=1
  ifc="$(basename "$net")"
  drv="$(basename "$(readlink -f "$net/device/driver" 2>/dev/null)" 2>/dev/null || echo '?')"
  phy="$(cat "$net/phy80211/name" 2>/dev/null || echo '?')"
  printf '  %s  (driver: %s, %s)\n' "$ifc" "$drv" "$phy"

  # Does this phy advertise any 5 GHz channel? (5xxx MHz lines)
  chan="$(iw phy "$phy" channels 2>/dev/null || iw phy "$phy" info 2>/dev/null)"
  is5=0
  if grep -qE '5[0-9]{3} MHz' <<<"$chan"; then
    is5=1; found5=1
    # DFS channels (52–144) are the secret weapon: public/cheap gear avoids them.
    if grep -qiE '5[0-9]{3} MHz.*(radar|DFS)' <<<"$chan"; then
      pass "$ifc supports 5 GHz — including DFS channels (near-private air)"
    else
      pass "$ifc supports 5 GHz"
    fi
  else
    if [ "$drv" = brcmfmac ]; then
      warn "$ifc is 2.4 GHz-only (this is the Pi's onboard radio)"
    else
      warn "$ifc is 2.4 GHz-only"
    fi
  fi

  # Can this phy run as an access point? The fallback hotspot needs it. Scan the
  # "Supported interface modes" block for a plain '* AP' entry (not AP/VLAN).
  ap=0
  if iw phy "$phy" info 2>/dev/null | awk '
       /Supported interface modes:/ { inmodes=1; next }
       inmodes {
         if ($0 ~ /^[[:space:]]*\*/) { if ($0 ~ /\* AP$/) found=1 }
         else { inmodes=0 }
       }
       END { exit(found?0:1) }'; then
    ap=1
    pass "$ifc supports AP mode (can host the fallback hotspot)"
  else
    warn "$ifc does NOT support AP mode"
  fi

  # Remember the best dongle candidate: a non-onboard interface that does 5 GHz.
  if [ "$is5" = 1 ] && [ "$drv" != brcmfmac ]; then
    dongle_iface="$ifc"; dongle_ap="$ap"
  fi
done
[ "$any_iface" = 1 ] || fail "No wlan interface found — dongle not enumerated (cable/power?)"

# ── 3. Verdict + the right layout for hqcam ──────────────────────────────────
bold "Verdict:"
di="${dongle_iface:-<dongle>}"
if [ "$found5" = 1 ] && [ "$verdict" = good ] && [ "$dongle_ap" = 1 ]; then
  pass "PASS — mainline 5 GHz dongle that also does AP mode."
  pass "Simplest layout: let the dongle be the only radio. Disable the onboard"
  pass "and the dongle becomes wlan0, so ALL hqcam config uses it unchanged"
  pass "(client + fallback AP). Edit /boot/firmware/config.txt, add:"
  echo  "      dtoverlay=disable-wifi"
  pass "then reboot. Confirm with: iw dev  and  nmcli device status"
elif [ "$found5" = 1 ] && [ -n "$dongle_iface" ]; then
  if [ "$dongle_ap" = 1 ]; then
    warn "USABLE — 5 GHz dongle present with AP mode, but verify the driver survives"
    warn "'sudo apt full-upgrade' + reboot before trusting it on a fleet. You can"
    warn "then disable onboard (dtoverlay=disable-wifi) so the dongle is wlan0."
  else
    warn "5 GHz present on '$di' but it does NOT do AP mode — so it can't host the"
    warn "fallback hotspot. Use the dual-radio layout: dongle = HQ client, onboard"
    warn "= AP fallback (this also lets client + AP run at once). Set it up with:"
    echo  "      sudo nmcli con modify hqcam-wifi connection.interface-name $di"
    echo  "      # keep AP on onboard:  /usr/local/etc/hqcam/hotspot.conf -> AP_IFACE=\"wlan0\""
    echo  "      # recorder online-check: /usr/local/etc/hqcam/record.conf -> HQCAM_WIFI_IFACE=$di"
    warn "Pin the dongle's name with a udev rule (by MAC) so wlan0/wlan1 don't swap."
  fi
else
  fail "NOT a 5 GHz dongle (or not enumerated). Get a Panda PAU09 / MT7612U."
fi
