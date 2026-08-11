#!/bin/sh
# Renders web.yml (basic auth) from env at boot. Prometheus's web config wants
# a bcrypt hash in a file, Railway hands us env vars; this is the adapter.
# PROM_WEB_BCRYPT is the bcrypt hash of PROM_REMOTE_WRITE_PASSWORD; the plain
# password is also needed in a file so our self-scrape can get through our own
# auth. Both env vars live only in Railway service variables.
set -eu
: "${PROM_WEB_BCRYPT:?set PROM_WEB_BCRYPT to the bcrypt hash of the remote-write password}"
: "${PROM_REMOTE_WRITE_PASSWORD:?set PROM_REMOTE_WRITE_PASSWORD to the plain remote-write password}"

cat > /etc/prometheus/web.yml <<EOF
basic_auth_users:
  remote: ${PROM_WEB_BCRYPT}
EOF
printf '%s' "${PROM_REMOTE_WRITE_PASSWORD}" > /etc/prometheus/password

exec prometheus \
  --config.file=/etc/prometheus/prometheus.yml \
  --storage.tsdb.path=/prometheus \
  --storage.tsdb.retention.time=30d \
  --web.enable-remote-write-receiver \
  --web.config.file=/etc/prometheus/web.yml \
  --web.listen-address=:9090
