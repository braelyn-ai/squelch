#!/bin/sh
# The broker's own default bind is loopback, which is correct for a box with a
# TLS proxy in front and unreachable inside a container. Railway injects PORT
# and terminates TLS at its edge, so bind whatever port it hands us on all
# interfaces — unless the operator pinned SQUELCH_BROKER_BIND themselves, which
# always wins.
#
# There is nothing else for this script to do: the broker has no volume and no
# state on disk, so the image already runs as the unprivileged `broker` user and
# exec keeps the broker PID 1, taking the platform's SIGTERM directly.
set -e

SQUELCH_BROKER_BIND="${SQUELCH_BROKER_BIND:-0.0.0.0:${PORT:-8851}}"
export SQUELCH_BROKER_BIND

exec /usr/local/bin/squelch-broker
