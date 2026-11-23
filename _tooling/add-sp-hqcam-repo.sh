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
echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${VPN_APT_BASE} stable hqcam"  | sudo tee    /etc/apt/sources.list.d/sp-hqcam.list
echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] ${PUBLIC_APT_BASE} stable hqcam" | sudo tee -a /etc/apt/sources.list.d/sp-hqcam.list

wget --no-check-certificate -vO - ${VPN_APT_BASE}/pubkey.gpg | sudo gpg --batch --yes --dearmor -o /etc/apt/keyrings/trusted.gpg
