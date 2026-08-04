#!/bin/sh
# Drop to `squelch` only after /data is writable by it.
#
# The daemon must not run as root, but a mounted volume arrives owned by root
# and the image's own ownership of the mount point is overlaid by the mount. So
# the one thing that has to happen as root is this chown, and then we hand off.
#
# setpriv (util-linux) replaces this process rather than forking, so squelchd
# is PID 1 and takes the platform's SIGTERM directly for graceful shutdown.
set -e

mkdir -p /data
# Best-effort: an already-correct volume, or one the platform pre-owns, must
# not turn into a boot failure here. The daemon reports it properly if the
# path is genuinely unusable.
chown -R squelch:squelch /data 2>/dev/null || true

exec setpriv --reuid=squelch --regid=squelch --init-groups /usr/local/bin/squelchd "$@"
