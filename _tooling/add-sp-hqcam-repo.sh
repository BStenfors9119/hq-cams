ARCH=$(dpkg --print-architecture)

PUBLIC_APT_URL=apt.sodapopsystems.com:1063
VPN_APT_URL=10.24.3.46:1063

# Camera-only repo file, kept separate from TSR's sp.list so the receivers' apt
# flow is never touched. Cameras only need the `hqcam` component; rpicam-apps
# comes from Pi OS's own repos.
#
# Modern keyring flow: apt-key is removed on Bookworm/trixie, so dearmor the
# repo public key into /etc/apt/keyrings and reference it via signed-by=
# (same key + keyring path as the TSR trixie script — one shared repo key).
sudo mkdir -p /etc/apt/keyrings
echo "Acquire::By-Hash=yes;" | sudo tee /etc/apt/apt.conf.d/99acquire-by-hash
echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] http://${VPN_APT_URL}/apt-repo stable hqcam"  | sudo tee    /etc/apt/sources.list.d/sp-hqcam.list
echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/trusted.gpg] http://${PUBLIC_APT_URL}/apt-repo stable hqcam" | sudo tee -a /etc/apt/sources.list.d/sp-hqcam.list

wget --no-check-certificate -vO - http://${VPN_APT_URL}/apt-repo/pubkey.gpg | sudo gpg --dearmor -o /etc/apt/keyrings/trusted.gpg
