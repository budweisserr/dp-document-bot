#!/usr/bin/env bash
#
# dpq — one-shot installer for Amazon Linux 2023 / RHEL-family x86_64.
#
#   sudo ./deploy.sh
#
# Installs the build dependencies, compiles the bot, installs it to /opt/dpq
# and runs it as the systemd service "dpq".
set -euo pipefail

APP_DIR=/opt/dpq
SERVICE=dpq
USER_NAME=dpq
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ ${EUID} -ne 0 ]]; then
    echo "run as root:  sudo $0" >&2
    exit 1
fi
if [[ ! -f $SRC_DIR/Cargo.toml ]]; then
    echo "Cargo.toml not found next to this script — run it from the project root" >&2
    exit 1
fi

echo "==> packages"
PKGS=(gcc gcc-c++ make cmake perl git tar gzip clang pkgconf-pkg-config pkgconf golang procps-ng which curl-minimal)
if ! dnf install -y "${PKGS[@]}"; then
    echo "   bulk install failed, retrying one by one"
    for p in "${PKGS[@]}"; do
        dnf install -y "$p" || echo "   (skipped $p)"
    done
fi
for c in gcc make cmake perl clang curl; do
    command -v "$c" >/dev/null || echo "!! warning: '$c' is missing"
done

echo "==> swap"
if [[ "$(awk '/SwapTotal/{print $2}' /proc/meminfo)" == "0" ]]; then
    MEM_MB=$(( $(awk '/MemTotal/{print $2}' /proc/meminfo) / 1024 ))
    if (( MEM_MB < 2048 )); then
        echo "   no swap and only ${MEM_MB} MB RAM — creating a 2G swapfile"
        fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048
        chmod 600 /swapfile
        mkswap /swapfile >/dev/null
        swapon /swapfile
        grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
    fi
fi

echo "==> rust toolchain"
export PATH="$HOME/.cargo/bin:$PATH"
export GOTOOLCHAIN=local
if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
fi

echo "==> compile (first run takes a few minutes: BoringSSL is built from source)"
cd "$SRC_DIR"
MEM_MB=$(( $(awk '/MemTotal/{print $2}' /proc/meminfo) / 1024 ))
if (( MEM_MB < 2048 )); then
    echo "   low RAM (${MEM_MB} MB) — limiting parallelism"
    export CMAKE_BUILD_PARALLEL_LEVEL=1
    export CARGO_BUILD_JOBS=1
    export CARGO_PROFILE_RELEASE_LTO=false
fi
cargo build --release --locked

echo "==> install to $APP_DIR"
id -u "$USER_NAME" >/dev/null 2>&1 \
    || useradd --system --home-dir "$APP_DIR" --shell /sbin/nologin "$USER_NAME"
install -d -o "$USER_NAME" -g "$USER_NAME" "$APP_DIR" "$APP_DIR/data"
install -m 0755 -o "$USER_NAME" -g "$USER_NAME" "$SRC_DIR/target/release/dpq" "$APP_DIR/dpq"
if [[ ! -f $APP_DIR/.env ]]; then
    install -m 0600 -o "$USER_NAME" -g "$USER_NAME" "$SRC_DIR/.env.example" "$APP_DIR/.env"
    echo "   wrote $APP_DIR/.env (BOT_TOKEN is empty)"
fi

echo "==> systemd unit"
cat > "/etc/systemd/system/${SERVICE}.service" <<EOF
[Unit]
Description=dpq — DP Document e-queue monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${USER_NAME}
Group=${USER_NAME}
WorkingDirectory=${APP_DIR}
EnvironmentFile=${APP_DIR}/.env
ExecStart=${APP_DIR}/dpq
Restart=always
RestartSec=10
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=${APP_DIR}/data

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable "$SERVICE"
systemctl restart "$SERVICE"

echo
echo "done."
echo "  status:  systemctl status $SERVICE"
echo "  logs:    journalctl -u $SERVICE -f"
echo "  config:  sudo nano $APP_DIR/.env  &&  sudo systemctl restart $SERVICE"
if ! grep -qE '^BOT_TOKEN=.+' "$APP_DIR/.env"; then
    echo
    echo "NOTE: BOT_TOKEN is empty — the bot runs in monitor-only mode (alerts in the log) until you set it."
fi
