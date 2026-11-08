#!/usr/bin/env bash
# One-shot provisioning script for Ubuntu 22.04 / 24.04 LTS on Vultr
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "error: run as root (sudo ./deploy/setup-vultr.sh)" >&2
  exit 1
fi

APP_DIR=${APP_DIR:-/opt/optionality}
DOMAIN=${DOMAIN:-} # leave empty for IP-only on port 80

echo "=== 1/7. Allocating 4GB swap space ==="
if [ ! -f /swapfile ]; then
  fallocate -l 4G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=4096
  chmod 600 /swapfile
  mkswap /swapfile
  swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
  echo "Swap configured: 4GB"
else
  echo "Swap already present"
fi

echo "=== 2/7. Installing system dependencies ==="
apt-get update
apt-get install -y --no-install-recommends \
  curl git build-essential pkg-config libssl-dev ufw lsof procps \
  debian-keyring debian-archive-keyring apt-transport-https ca-certificates

echo "=== 3/7. Installing Caddy (reverse proxy + automatic TLS) ==="
if ! command -v caddy &>/dev/null; then
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list
  apt-get update
  apt-get install -y caddy
fi

echo "=== 4/7. Installing Node.js 22 LTS ==="
if ! command -v node &>/dev/null; then
  curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
  apt-get install -y nodejs
fi

echo "=== 5/7. Installing Rust toolchain ==="
if ! command -v cargo &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
export PATH="/root/.cargo/bin:$PATH"
rustup default stable
rustup target add wasm32-unknown-unknown || true

echo "=== 6/7. Building Optionality release binaries ==="
cd "$APP_DIR/near-mvp"
npm install
node sim/build-demo.mjs
cargo build --release -p engine -p chain-host -p oracle-signer

echo "=== 7/7. Configuring systemd and Caddy ==="
cp deploy/optionality.service /etc/systemd/system/optionality.service
sed -i "s|/opt/optionality|$APP_DIR|g" /etc/systemd/system/optionality.service

if [ -n "$DOMAIN" ]; then
  cat > /etc/caddy/Caddyfile <<EOF
$DOMAIN {
    reverse_proxy 127.0.0.1:8790 {
        flush_interval -1
    }
}
EOF
else
  cat > /etc/caddy/Caddyfile <<EOF
:80 {
    reverse_proxy 127.0.0.1:8790 {
        flush_interval -1
    }
}
EOF
fi

systemctl daemon-reload
systemctl enable --now optionality
systemctl restart caddy

ufw allow 22/tcp
ufw allow 80/tcp
ufw allow 443/tcp
ufw --force enable

echo
echo "=== Deployment Complete ==="
echo "Status: systemctl status optionality"
echo "Logs:   journalctl -u optionality -f"
