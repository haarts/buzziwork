#!/bin/sh
# Build on the server and install buzziwork as a systemd user service. No root needed.
#   deploy/deploy.sh            # HOST defaults to racknerd
# Builds inside a rust:1-bookworm container on the server (needs docker there), because a
# binary built on a newer-glibc dev machine won't run on the server.
# Layout on the server:
#   ~/.local/bin/{buzziwork,signal-cli}
#   ~/.config/buzziwork/env          (created once from .env.example, chmod 600)
#   ~/.local/share/buzziwork/        (working dir, SQLite file)
#   ~/.local/share/signal-cli/       (signal-cli default data dir)
set -eu
HOST=${HOST:-racknerd}
SIGNAL_CLI_VERSION=${SIGNAL_CLI_VERSION:-0.14.9}
cd "$(dirname "$0")/.."

rsync -az --delete --exclude target --exclude .git --exclude .env --exclude '*.db' \
  ./ "$HOST":buzziwork-src/

ssh "$HOST" "SIGNAL_CLI_VERSION=$SIGNAL_CLI_VERSION sh -s" <<'EOF'
set -eu
cd ~/buzziwork-src
docker volume create buzziwork-cargo >/dev/null
docker run --rm -v "$PWD":/src -v buzziwork-cargo:/usr/local/cargo/registry -w /src \
  rust:1-bookworm sh -c "cargo build --release && chown -R $(id -u):$(id -g) target"

mkdir -p ~/.local/bin ~/.config/buzziwork ~/.local/share/buzziwork ~/.config/systemd/user
install -m 755 target/release/buzziwork ~/.local/bin/buzziwork

if ! ~/.local/bin/signal-cli --version 2>/dev/null | grep -qx "signal-cli $SIGNAL_CLI_VERSION"; then
  echo ">> installing signal-cli $SIGNAL_CLI_VERSION"
  tmp=$(mktemp -d)
  curl -sSL "https://github.com/AsamK/signal-cli/releases/download/v$SIGNAL_CLI_VERSION/signal-cli-$SIGNAL_CLI_VERSION-Linux-native.tar.gz" \
    | tar xz -C "$tmp"
  install -m 755 "$tmp/signal-cli" ~/.local/bin/signal-cli
  rm -rf "$tmp"
fi

install -m 644 deploy/buzziwork.service ~/.config/systemd/user/buzziwork.service
if [ ! -e ~/.config/buzziwork/env ]; then
  sed "s/^BOARD_TOKEN=.*/BOARD_TOKEN=$(openssl rand -hex 16)/" .env.example > ~/.config/buzziwork/env
  chmod 600 ~/.config/buzziwork/env
  echo ">> created ~/.config/buzziwork/env: fill in OPENROUTER_API_KEY, SIGNAL_ACCOUNT, GROUP_ID"
fi

loginctl show-user "$USER" -p Linger | grep -q yes || loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable buzziwork >/dev/null 2>&1
if systemctl --user is-active -q buzziwork; then
  systemctl --user restart buzziwork && echo ">> restarted"
else
  echo ">> installed; start with: systemctl --user start buzziwork"
fi
EOF
