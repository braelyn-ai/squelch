#!/usr/bin/env bash
# Update squelchd on baddiebox and restart it, in one command.
#
#   sudo ./deploy/update.sh            # pull latest, rebuild, install, restart
#   sudo ./deploy/update.sh --no-pull  # skip git pull (build the working tree as-is)
#
# Run from a checkout of the repo on the box. Rebuilds the release binary, drops
# it at /usr/local/bin/squelchd, restarts the systemd unit, and tails the
# startup line so you can confirm both doors came back up. Idempotent and safe
# to re-run.
set -euo pipefail

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

echo "==> install $BIN"
install -m 0755 target/release/squelchd "$BIN"

echo "==> restart $UNIT"
systemctl restart "$UNIT"

# Give it a moment to bind, then show the truth.
sleep 2
systemctl --no-pager --lines=0 status "$UNIT" || true
echo "==> recent log (startup line should name both doors):"
journalctl -u "$UNIT" --no-pager --lines=8
