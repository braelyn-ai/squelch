#!/bin/sh
# Drop to `control` only after the control store's directory is writable by it.
#
# The control plane must not run as root, but a mounted volume arrives owned by
# root and the image's own ownership of the mount point is overlaid by the
# mount. So the one thing that has to happen as root is this chown, and then we
# hand off.
#
# setpriv (util-linux) replaces this process rather than forking, so the server
# is PID 1 and takes the platform's SIGTERM directly.
set -e

if [ -n "${SQUELCH_CONTROL_DB_PATH:-}" ]; then
    dir=$(dirname "$SQUELCH_CONTROL_DB_PATH")
    mkdir -p "$dir"
    # Best-effort: an already-correct volume, or one the platform pre-owns,
    # must not turn into a boot failure here. The server reports it properly if
    # the path is genuinely unusable.
    chown -R control:control "$dir" 2>/dev/null || true
fi

# The control plane's own default bind is loopback, which is correct for a box
# with a TLS proxy in front and unreachable inside a container. Railway injects
# PORT and terminates TLS at its edge, so bind whatever port it hands us on all
# interfaces unless the operator pinned SQUELCH_CONTROL_BIND themselves.
SQUELCH_CONTROL_BIND="${SQUELCH_CONTROL_BIND:-0.0.0.0:${PORT:-8852}}"
export SQUELCH_CONTROL_BIND

exec setpriv --reuid=control --regid=control --init-groups /usr/local/bin/squelch-control "$@"
