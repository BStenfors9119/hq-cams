#!/bin/bash
#
# hqcam Pi first-run provisioning script.
#
# Intended to be baked onto the boot partition and wired into a self-disabling
# systemd one-shot by ../../../_scripts/new-hqcam-pi.sh (via make-firstrun.sh).
# It runs ONCE, as root, on the very first boot of a freshly imaged Pi, and:
#
#   1. Creates an admin user + enables SSH (password auth).
#   2. Joins HQ WiFi (so the cam reaches Motion Eye and can sync footage).
#   3. Waits for the network, fixes DNS, and syncs the clock (RTC-less Pis).
#   4. Adds the SodaPop `hqcam` apt component and installs the `hqcam` package
#      (pulls in rpicam-apps, NetworkManager, rsync — the camera stack).
#   5. Seeds the fallback-hotspot and footage-sync configs from baked values, so
#      the card boots a fully working camera.
#
# Unlike a TSR receiver there is NO venue/location registration — a camera is
# self-contained. Values below are substituted by make-firstrun.sh; any left
# unsubstituted fall back to a boot-partition file or the package defaults.

set -u

# ── Logging ────────────────────────────────────────────────────────────────
# Everything goes to a log on disk AND, best-effort, to the boot partition so it
# can be read by pulling the SD card if the Pi never comes online.
LOG_FILE="/var/log/hqcam-firstrun.log"
exec > >(tee -a "$LOG_FILE") 2>&1
echo "=== hqcam first-run started: $(date -u) ==="

# ── Locate the boot partition (Bookworm: /boot/firmware, older: /boot) ───────
BOOT_DIR="/boot/firmware"
[ -d "$BOOT_DIR" ] || BOOT_DIR="/boot"
echo "Boot partition: $BOOT_DIR"
ln -sf "$LOG_FILE" "$BOOT_DIR/hqcam-firstrun.log" 2>/dev/null || true

# ── Admin user + SSH (SSH on with password auth) ─────────────────────────────
# The password is baked as a crypt hash by make-firstrun.sh (never plaintext on
# the FAT boot partition). If it wasn't baked, the password is left unchanged.
ADMIN_USER='__HQCAM_ADMIN_USER__'
ADMIN_PWHASH='__HQCAM_ADMIN_PWHASH__'
[[ "$ADMIN_USER" == __HQCAM_* || -z "$ADMIN_USER" ]] && ADMIN_USER="hqadmin"

ensure_user_and_ssh() {
    if ! id "$ADMIN_USER" >/dev/null 2>&1; then
        useradd -m -s /bin/bash "$ADMIN_USER"
        echo "Created user $ADMIN_USER."
    else
        echo "User $ADMIN_USER already exists."
    fi

    # Add to the groups the default pi user normally has (only ones that exist).
    for g in sudo adm dialout cdrom audio video plugdev games users input render netdev gpio i2c spi; do
        getent group "$g" >/dev/null 2>&1 && usermod -aG "$g" "$ADMIN_USER"
    done

    if [[ "$ADMIN_PWHASH" == \$* ]]; then
        echo "$ADMIN_USER:$ADMIN_PWHASH" | chpasswd -e
        echo "Set $ADMIN_USER password from baked hash."
    else
        echo "WARNING: no password hash baked; $ADMIN_USER password left unchanged." >&2
    fi

    echo "$ADMIN_USER ALL=(ALL) NOPASSWD: ALL" > "/etc/sudoers.d/010_${ADMIN_USER}-nopasswd"
    chmod 440 "/etc/sudoers.d/010_${ADMIN_USER}-nopasswd"

    if command -v raspi-config >/dev/null 2>&1; then
        raspi-config nonint do_ssh 0 || true
    else
        systemctl enable --now ssh 2>/dev/null \
            || systemctl enable --now sshd 2>/dev/null || true
    fi
    mkdir -p /etc/ssh/sshd_config.d
    printf 'PasswordAuthentication yes\nUsePAM yes\n' > /etc/ssh/sshd_config.d/99-hqcam.conf
    systemctl reload ssh 2>/dev/null || systemctl restart ssh 2>/dev/null || true

    # Stop the Bookworm first-boot user wizard from fighting us.
    systemctl disable userconfig.service 2>/dev/null || true
}
ensure_user_and_ssh

# ── HQ WiFi (so the cam reaches Motion Eye + can sync footage) ────────────────
# Resolved in priority order:
#   1. Baked placeholders (make-firstrun.sh --wifi-ssid/--wifi-psk), else
#   2. a `hqcam-wifi.txt` file on the boot partition with lines:
#        ssid=Some Network
#        psk=thepassword
#        country=US        (optional, defaults to US)
# With no SSID the cam still works: on boot it can't find HQ WiFi and brings up
# its own fallback AP (hqcam-hotspot), viewable at http://10.42.0.1:6877.
WIFI_SSID='__HQCAM_WIFI_SSID__'
WIFI_PSK='__HQCAM_WIFI_PSK__'
WIFI_COUNTRY='__HQCAM_WIFI_COUNTRY__'

configure_wifi() {
    [[ "$WIFI_SSID" == __HQCAM_* ]] && WIFI_SSID=""
    [[ "$WIFI_PSK"  == __HQCAM_* ]] && WIFI_PSK=""
    [[ "$WIFI_COUNTRY" == __HQCAM_* ]] && WIFI_COUNTRY=""

    if [ -z "$WIFI_SSID" ] && [ -f "$BOOT_DIR/hqcam-wifi.txt" ]; then
        echo "Reading WiFi from $BOOT_DIR/hqcam-wifi.txt"
        while IFS='=' read -r key val; do
            key="$(echo "$key" | tr -d '[:space:]')"
            case "$key" in
                ssid)    WIFI_SSID="${val# }" ;;
                psk)     WIFI_PSK="${val# }" ;;
                country) WIFI_COUNTRY="$(echo "$val" | tr -d '[:space:]')" ;;
            esac
        done < "$BOOT_DIR/hqcam-wifi.txt"
    fi

    if [ -z "$WIFI_SSID" ]; then
        echo "No HQ WiFi config provided; the fallback AP will take over if HQ is unreachable."
        return 0
    fi
    [ -n "$WIFI_COUNTRY" ] || WIFI_COUNTRY="US"
    echo "Configuring HQ WiFi for SSID '$WIFI_SSID' (country $WIFI_COUNTRY)."

    if command -v raspi-config >/dev/null 2>&1; then
        raspi-config nonint do_wifi_country "$WIFI_COUNTRY" || true
    fi
    command -v rfkill >/dev/null 2>&1 && rfkill unblock wifi || true

    # hqcam is NetworkManager-only (the fallback hotspot needs it). Prefer nmcli;
    # fall back to wpa_supplicant only on an older non-NM image.
    if command -v nmcli >/dev/null 2>&1 && systemctl is-active --quiet NetworkManager; then
        nmcli con delete "hqcam-wifi" >/dev/null 2>&1 || true
        nmcli con add type wifi con-name "hqcam-wifi" ifname wlan0 ssid "$WIFI_SSID" || true
        nmcli con modify "hqcam-wifi" \
            wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$WIFI_PSK" \
            connection.autoconnect yes || true
        nmcli con up "hqcam-wifi" || true
    else
        local WPA="/etc/wpa_supplicant/wpa_supplicant.conf"
        {
            echo "country=$WIFI_COUNTRY"
            echo "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev"
            echo "update_config=1"
            echo "network={"
            echo "    ssid=\"$WIFI_SSID\""
            echo "    psk=\"$WIFI_PSK\""
            echo "}"
        } > "$WPA"
        chmod 600 "$WPA"
        wpa_cli -i wlan0 reconfigure 2>/dev/null \
            || systemctl restart "wpa_supplicant@wlan0" 2>/dev/null || true
    fi
}
configure_wifi

# ── Wait for raw network connectivity (IP only, not DNS) ─────────────────────
echo "Waiting for network connectivity..."
net_up=""
for i in $(seq 1 60); do
    if ping -c1 -W2 8.8.8.8 >/dev/null 2>&1; then
        echo "Network is up (after ${i} tries)."
        net_up="yes"; break
    fi
    sleep 5
done
[ -n "$net_up" ] || echo "WARNING: no IP connectivity after 300s; continuing anyway." >&2

# ── Make sure DNS actually resolves the apt mirror ───────────────────────────
APT_HOST="apt.sodapopsystems.com"
FALLBACK_DNS="8.8.8.8 1.1.1.1"

resolves() { getent hosts "$1" >/dev/null 2>&1; }
wait_resolve() {  # wait_resolve <host> <tries>  (5s apart)
    local host="$1" tries="$2" i
    for i in $(seq 1 "$tries"); do
        resolves "$host" && { echo "DNS ok for $host (try ${i})."; return 0; }
        sleep 5
    done
    return 1
}
ensure_dns() {
    local host="$1"
    wait_resolve "$host" 12 && return 0
    echo "DNS not resolving '$host'; forcing fallback resolvers ($FALLBACK_DNS)." >&2
    if command -v nmcli >/dev/null 2>&1 && systemctl is-active --quiet NetworkManager; then
        local active
        active="$(nmcli -t -f NAME con show --active 2>/dev/null | head -1)"
        [ -n "$active" ] || active="hqcam-wifi"
        nmcli con modify "$active" ipv4.dns "$FALLBACK_DNS" ipv4.ignore-auto-dns yes 2>/dev/null || true
        nmcli con up "$active" 2>/dev/null || true
    fi
    if systemctl is-active --quiet systemd-resolved 2>/dev/null; then
        mkdir -p /etc/systemd/resolved.conf.d
        printf '[Resolve]\nDNS=8.8.8.8 1.1.1.1\nFallbackDNS=9.9.9.9\n' \
            > /etc/systemd/resolved.conf.d/hqcam-fallback.conf
        systemctl restart systemd-resolved 2>/dev/null || true
    fi
    { echo "nameserver 8.8.8.8"; echo "nameserver 1.1.1.1"; } > /etc/resolv.conf 2>/dev/null || true
    wait_resolve "$host" 12 && return 0
    echo "WARNING: still cannot resolve '$host' after fallback; apt install may fail." >&2
    return 1
}
ensure_dns "$APT_HOST"

# ── Wait for the clock to sync ───────────────────────────────────────────────
# RTC-less Pis (Zero W/2 W) boot at the image's build date until NTP corrects
# them. A clock stuck in the past makes apt reject every repo ("Release file is
# not valid yet"), so sync BEFORE touching apt.
echo "Syncing system clock (was: $(date -u))..."
timedatectl set-ntp true 2>/dev/null || true
systemctl restart systemd-timesyncd 2>/dev/null || true
clock_ok=""
for i in $(seq 1 30); do
    if [ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null)" = "yes" ]; then
        clock_ok="yes"; break
    fi
    sleep 2
done
if [ -z "$clock_ok" ]; then
    echo "NTP didn't sync in 60s; setting clock from an HTTP Date header."
    http_date="$(wget --timeout=10 --tries=1 -qSO /dev/null http://google.com 2>&1 | sed -n 's/^[[:space:]]*Date:[[:space:]]*//p' | head -1)"
    [ -n "$http_date" ] && date -s "$http_date" >/dev/null 2>&1 || \
        echo "WARNING: could not set clock; apt may reject repos as 'not valid yet'." >&2
fi
echo "Clock now: $(date -u)"

# ── Temporary zram swap to survive apt on a low-RAM Pi Zero ───────────────────
# hqcam itself is a lean Rust binary, but rpicam-apps + NetworkManager pulled in
# on a fresh Lite image can still spike RAM on a 512MB single-core Zero W. A
# COMPRESSED RAM swap (zram) gives headroom at RAM speed. Install-time only;
# all best-effort so a missing zram module never aborts the first-run.
setup_install_zram() {
    local mem_kb mem_mb
    mem_kb="$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null)"
    [ -n "$mem_kb" ] || { echo "Could not read MemTotal; skipping install-time zram."; return 0; }
    mem_mb=$(( mem_kb / 1024 ))
    if [ "$mem_mb" -gt 768 ]; then
        echo "RAM ${mem_mb}MB > 768MB; skipping install-time zram (not needed)."
        return 0
    fi
    echo "Low RAM (${mem_mb}MB) — enabling temporary zram swap for the apt install."
    modprobe zram 2>/dev/null || { echo "WARNING: zram module unavailable; continuing." >&2; return 0; }
    [ -b /dev/zram0 ] || { echo "WARNING: /dev/zram0 missing after modprobe; skipping." >&2; return 0; }
    swapoff /dev/zram0 2>/dev/null || true
    echo 1 > /sys/block/zram0/reset 2>/dev/null || true
    echo lz4 > /sys/block/zram0/comp_algorithm 2>/dev/null || true
    echo "$(( mem_mb / 2 ))M" > /sys/block/zram0/disksize 2>/dev/null || true
    sysctl -w vm.swappiness=100 >/dev/null 2>&1 || true
    if mkswap /dev/zram0 >/dev/null 2>&1 && swapon -p 100 /dev/zram0 2>/dev/null; then
        echo "zram swap active:"; swapon --show 2>/dev/null || true
    else
        echo "WARNING: failed to enable zram swap; continuing without it." >&2
    fi
}
setup_install_zram

# ── Add the SodaPop `hqcam` apt component and install hqcam ───────────────────
# Camera-only repo file, kept separate from any TSR sp.list. Modern keyring flow
# (apt-key is gone on Bookworm/trixie): dearmor the repo key into
# /etc/apt/keyrings and reference it via signed-by=. Two mirrors in priority
# order — the VPN/LAN host (fast, at HQ) then the public HTTPS mirror (fallback
# in the field), with apt timeouts/retries added so an unroutable VPN mirror out
# in the field doesn't stall the install.
ARCH="$(dpkg --print-architecture)"
VPN_APT_BASE="http://10.24.3.46:1063/apt-repo"
PUBLIC_APT_BASE="https://apt.sodapopsystems.com/apt-repo"
echo "Acquire::By-Hash=yes;" > /etc/apt/apt.conf.d/99acquire-by-hash
cat > /etc/apt/apt.conf.d/80-hqcam-net <<'EOF'
Acquire::Retries "3";
Acquire::http::Timeout "20";
Acquire::https::Timeout "20";
EOF
mkdir -p /etc/apt/keyrings
{
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${VPN_APT_BASE} stable hqcam"
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${PUBLIC_APT_BASE} stable hqcam"
} > /etc/apt/sources.list.d/sp-hqcam.list
# Same signing key for both mirrors — import it, trying the VPN host first then
# public. --timeout/--tries keep the unroutable VPN host from hanging wget.
WGET_OPTS="--timeout=10 --tries=1 --no-check-certificate -qO -"
if   wget $WGET_OPTS "${VPN_APT_BASE}/pubkey.gpg" 2>/dev/null | gpg --batch --yes --dearmor -o /etc/apt/keyrings/trusted.gpg 2>/dev/null && [ -s /etc/apt/keyrings/trusted.gpg ]; then
    echo "Imported apt key from VPN mirror."
elif wget $WGET_OPTS "${PUBLIC_APT_BASE}/pubkey.gpg" 2>/dev/null | gpg --batch --yes --dearmor -o /etc/apt/keyrings/trusted.gpg 2>/dev/null && [ -s /etc/apt/keyrings/trusted.gpg ]; then
    echo "Imported apt key from public mirror."
else
    echo "WARNING: failed to import apt key; install may fail." >&2
fi

echo "Updating apt and installing hqcam..."
export DEBIAN_FRONTEND=noninteractive
sudo apt update

# Recover from a previously interrupted install before retrying. firstrun re-runs
# on every boot until it exits 0, so heal a half-configured dpkg DB first (both
# no-ops on a clean system) so the retry completes.
sudo dpkg --configure -a || true
sudo apt install -f -y || true

if ! sudo apt install -y hqcam; then
    echo "ERROR: 'apt install hqcam' failed. See $LOG_FILE." >&2
    exit 1
fi

# ── Fallback hotspot: DEACTIVATED ─────────────────────────────────────────────
# The AP held the radio and kept cams from rejoining HQ WiFi. Newer debs no longer
# enable the timer, but older ones in the repo still do — so turn it off here
# regardless of which deb was installed, and drop the AP if it already came up.
echo "Deactivating the fallback hotspot (hqcam-hotspot.timer)..."
systemctl disable --now hqcam-hotspot.timer 2>/dev/null || true
systemctl stop hqcam-hotspot.service 2>/dev/null || true
/usr/local/bin/hqcam-hotspot off 2>/dev/null || true

# ── Seed the fallback-hotspot + footage-sync configs from baked values ────────
# The deb installs hotspot.conf/sync.conf on first install only (as .default,
# copied if absent), so overwrite the specific keys here rather than the files.
AP_SSID='__HQCAM_AP_SSID__'
AP_PASSWORD='__HQCAM_AP_PASSWORD__'
SYNC_DEST='__HQCAM_SYNC_DEST__'
SYNC_ENABLED='__HQCAM_SYNC_ENABLED__'
[[ "$AP_SSID" == __HQCAM_* ]] && AP_SSID=""
[[ "$AP_PASSWORD" == __HQCAM_* ]] && AP_PASSWORD=""
[[ "$SYNC_DEST" == __HQCAM_* ]] && SYNC_DEST=""
[[ "$SYNC_ENABLED" == __HQCAM_* ]] && SYNC_ENABLED=""

# set_kv <file> <KEY> <value> — replace a whole `KEY=...` line, quoting the value.
set_kv() {
    local file="$1" key="$2" value="$3"
    [ -f "$file" ] || return 0
    local esc; esc="$(printf '%s' "$value" | sed -e 's/[&|\\]/\\&/g')"
    if grep -qE "^[[:space:]]*${key}=" "$file"; then
        sed -i "s|^[[:space:]]*${key}=.*|${key}=\"${esc}\"|" "$file"
    else
        printf '%s="%s"\n' "$key" "$value" >> "$file"
    fi
}

HOTSPOT_CONF="/usr/local/etc/hqcam/hotspot.conf"
if [ -n "$AP_SSID" ] || [ -n "$AP_PASSWORD" ]; then
    echo "Seeding fallback-hotspot config ($HOTSPOT_CONF)..."
    [ -n "$AP_SSID" ]     && set_kv "$HOTSPOT_CONF" AP_SSID "$AP_SSID"
    [ -n "$AP_PASSWORD" ] && set_kv "$HOTSPOT_CONF" AP_PASSWORD "$AP_PASSWORD"
    # Hotspot DEACTIVATED (it held the radio and blocked HQ rejoin) — config is
    # still seeded for manual `hqcam-hotspot on`, but don't start it here.
    # systemctl restart hqcam-hotspot.service 2>/dev/null || true
else
    echo "No AP overrides baked; leaving package default hotspot.conf."
fi

SYNC_CONF="/usr/local/etc/hqcam/sync.conf"
if [ -n "$SYNC_DEST" ]; then
    echo "Seeding footage-sync config ($SYNC_CONF)..."
    set_kv "$SYNC_CONF" SYNC_DEST "$SYNC_DEST"
    set_kv "$SYNC_CONF" SYNC_ENABLED "${SYNC_ENABLED:-yes}"
    systemctl restart hqcam-sync.timer 2>/dev/null || true
else
    echo "No sync destination baked; leaving footage-sync disabled (edit $SYNC_CONF later)."
fi

# ── Make sure the camera service is up ───────────────────────────────────────
# The deb's postinst enables+starts hqcam and the sync timer (the hotspot timer
# is deactivated — see postinst).
# Nudge the stream service in case config seeding raced it.
systemctl enable hqcam 2>/dev/null || true
systemctl restart hqcam 2>/dev/null || true

echo "=== hqcam first-run finished: $(date -u) ==="
echo "Stream: http://<pi-ip>:6877/  (or the fallback AP at http://10.42.0.1:6877/)"
exit 0
