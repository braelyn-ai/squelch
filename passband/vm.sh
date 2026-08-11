#!/usr/bin/env bash
# vm.sh — boot a throwaway macOS VM running the latest Passband build, with no
# settings whatsoever: the true first-run experience, on demand.
#
# Every run clones the pristine Tahoe base image, so the guest has no
# UserDefaults, no keychain entry, no notification grant, no daemon — exactly
# what a new user sees. The clone is torn down when the VM window closes (or on
# ctrl-C), so the next run is pristine again.
#
#   ./vm.sh              build, boot, launch Passband inside the VM
#   ./vm.sh --no-build   reuse build/Passband.app as-is
#   ./vm.sh --keep       keep the VM after closing (resume: tart run passband-firstrun)
#   ./vm.sh --daemon     reverse-tunnel the host's squelchd into the guest, so
#                        the connect gate accepts http://127.0.0.1:8848
#
# The first ever run pulls the ~40GB base image (cirruslabs/macos-tahoe-base);
# after that the clone comes from the local cache in seconds. Guest login is
# admin/admin if you need a shell.

set -euo pipefail
cd "$(dirname "$0")"

IMAGE="ghcr.io/cirruslabs/macos-tahoe-base:latest"
VM="passband-firstrun"

BUILD=1 KEEP=0 DAEMON=0
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --keep) KEEP=1 ;;
    --daemon) DAEMON=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

for tool in tart sshpass; do
  command -v "$tool" >/dev/null 2>&1 \
    || { echo "missing $tool — brew install cirruslabs/cli/tart sshpass" >&2; exit 1; }
done

[[ $BUILD == 1 ]] && ./build.sh
[[ -d build/Passband.app ]] || { echo "no build/Passband.app — run ./build.sh" >&2; exit 1; }

# A leftover from --keep is stale twice over: old build, dirtied state.
tart get "$VM" >/dev/null 2>&1 && tart delete "$VM"
echo "==> cloning $IMAGE"
tart clone "$IMAGE" "$VM"
tart set "$VM" --display 1920x1200

echo "==> booting"
tart run "$VM" &
RUN_PID=$!

TUNNEL_PID=""
cleanup() {
  # set -e is fatal inside an EXIT trap: the first no-op kill would abort the
  # trap and strand the VM. Nothing below is allowed to end the funeral early.
  set +e
  [[ -n $TUNNEL_PID ]] && kill "$TUNNEL_PID" 2>/dev/null
  kill "$RUN_PID" 2>/dev/null
  wait "$RUN_PID" 2>/dev/null
  if [[ $KEEP == 1 ]]; then
    echo "==> kept — resume with: tart run $VM"
  else
    tart delete "$VM" 2>/dev/null
    echo "==> $VM deleted; next run starts pristine"
  fi
  return 0
}
trap cleanup EXIT

guest() {
  sshpass -p admin ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -o ConnectTimeout=5 admin@"$IP" "$@"
}

echo "==> waiting for the guest (a fresh boot takes a minute or two)"
IP=""
for _ in $(seq 1 60); do
  kill -0 "$RUN_PID" 2>/dev/null || { echo "tart run exited early" >&2; exit 1; }
  if IP=$(tart ip "$VM" 2>/dev/null) && [[ -n $IP ]] && guest true 2>/dev/null; then
    break
  fi
  IP=""
  sleep 5
done
[[ -n $IP ]] || { echo "guest never answered ssh" >&2; exit 1; }

# tar over ssh, not a virtiofs share: cp out of the mount trips over the
# framework symlinks inside Sparkle, and a streamed archive lands like a real
# install anyway. No quarantine xattr, so Gatekeeper has nothing to interrupt.
echo "==> installing Passband in the guest ($IP)"
tar -C build -cf - Passband.app | guest 'tar -C "$HOME/Desktop" -xf -'
guest 'open "$HOME/Desktop/Passband.app"'

if [[ $DAEMON == 1 ]]; then
  sshpass -p admin ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -N -R 8848:127.0.0.1:8848 admin@"$IP" &
  TUNNEL_PID=$!
  echo "==> host squelchd tunneled — connect gate URL: http://127.0.0.1:8848"
fi

echo "==> first-run lab is up; close the VM window when you're done"
wait "$RUN_PID" || true
