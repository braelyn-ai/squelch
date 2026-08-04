#!/bin/sh
# Drop to `relay` only after the open buffer's directory is writable by it.
#
# The relay must not run as root, but a mounted volume arrives owned by root and
# the image's own ownership of the mount point is overlaid by the mount. So the
# one thing that has to happen as root is this chown, and then we hand off.
#
# setpriv (util-linux) replaces this process rather than forking, so the relay is
# PID 1 and takes the platform's SIGTERM directly.
set -e

if [ -n "${SQUELCH_RELAY_DB_PATH:-}" ]; then
    dir=$(dirname "$SQUELCH_RELAY_DB_PATH")
    mkdir -p "$dir"
    # Best-effort: an already-correct volume, or one the platform pre-owns, must
    # not turn into a boot failure here. The relay reports it properly if the
    # path is genuinely unusable.
    chown -R relay:relay "$dir" 2>/dev/null || true
fi

# Railway injects PORT and terminates TLS at its edge; bind whatever it gives us
# on all interfaces unless the operator pinned SQUELCH_RELAY_BIND themselves.
SQUELCH_RELAY_BIND="${SQUELCH_RELAY_BIND:-0.0.0.0:${PORT:-8850}}"
export SQUELCH_RELAY_BIND

exec setpriv --reuid=relay --regid=relay --init-groups /usr/local/bin/squelch-relay
