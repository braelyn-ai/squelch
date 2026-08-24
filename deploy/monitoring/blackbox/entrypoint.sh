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

# The model the canary probes. MUST equal what the warden hands tenant daemons
# (SQUELCH_WARDEN_LLM_STAGE1_MODEL / _STAGE2_MODEL); a canary probing a model
# the fleet does not send is a canary that can go green while the fleet is
# down. Provider-qualified by default: this gateway cannot auto-resolve a bare
# `claude-opus-5`.
MODEL="${BLACKBOX_CANARY_MODEL:-anthropic/claude-opus-5}"
echo "blackbox: bifrost_llm canary will probe model ${MODEL}" >&2

# Rendered to /tmp: the stock image runs as nobody, which cannot write
# /etc/blackbox_exporter.
sed -e "s|__BIFROST_CANARY_KEY__|${KEY}|" \
    -e "s|__CANARY_MODEL__|${MODEL}|" \
  /etc/blackbox_exporter/config.template.yml > /tmp/config.yml

exec /bin/blackbox_exporter --config.file=/tmp/config.yml
