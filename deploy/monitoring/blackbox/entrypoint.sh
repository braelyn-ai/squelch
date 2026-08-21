#!/bin/sh
# Renders the blackbox config from env at boot: the bifrost_llm canary module
# needs the canary virtual key, which lives only in Railway service variables
# (BLACKBOX_BIFROST_CANARY_KEY). Same adapter pattern as prometheus's web.yml.
#
# An UNSET key must not stop the exporter: the other probes (site/warden
# uptime) matter more than the canary, so we render a dead placeholder and let
# the canary probe fail visibly instead.
set -eu

KEY="${BLACKBOX_BIFROST_CANARY_KEY:-unset-canary-key}"
if [ "$KEY" = "unset-canary-key" ]; then
  echo "blackbox: BLACKBOX_BIFROST_CANARY_KEY is unset; the bifrost_llm canary will fail until it is configured" >&2
fi

# Rendered to /tmp: the stock image runs as nobody, which cannot write
# /etc/blackbox_exporter.
sed "s|__BIFROST_CANARY_KEY__|${KEY}|" \
  /etc/blackbox_exporter/config.template.yml > /tmp/config.yml

exec /bin/blackbox_exporter --config.file=/tmp/config.yml
