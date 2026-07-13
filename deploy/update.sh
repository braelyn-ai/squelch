#!/usr/bin/env bash
# Update squelchd on baddiebox and restart it, in one command.
#
#   ./deploy/update.sh            # pull latest, rebuild, install, restart
#   ./deploy/update.sh --no-pull  # skip git pull (build the working tree as-is)
#
# Run this as your normal user (NOT with sudo). git pull and cargo build run as
# you — using your SSH keys, git config, and cargo cache — and only the two
# privileged steps (install the binary, restart the unit) elevate via sudo, so
# you get a single password prompt. Running the whole thing under sudo would use
# root's HOME/keys and leave root-owned files in target/; refuse that.
set -euo pipefail

if [[ ${EUID:-$(id -u)} -eq 0 ]]; then
  echo "error: run this as your normal user, not with sudo." >&2
  echo "       (it elevates only the install + restart steps itself.)" >&2
  exit 1
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN=/usr/local/bin/squelchd
UNIT=squelchd
PULL=1

for arg in "$@"; do
  case "$arg" in
    --no-pull) PULL=0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

cd "$REPO"

if [[ $PULL -eq 1 ]]; then
  echo "==> git pull"
  git pull --ff-only
fi

echo "==> cargo build --release -p squelchd"
cargo build --release -p squelchd

echo "==> install $BIN (sudo)"
sudo install -m 0755 target/release/squelchd "$BIN"

echo "==> restart $UNIT (sudo)"
sudo systemctl restart "$UNIT"

# Give it a moment to bind, then show the truth.
sleep 2
sudo systemctl --no-pager --lines=0 status "$UNIT" || true
echo "==> recent log (startup line should name both doors):"
sudo journalctl -u "$UNIT" --no-pager --lines=8
