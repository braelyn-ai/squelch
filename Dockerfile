# Builds squelch-relay — the blind APNs push relay. It is the only piece of
# squelch that runs on shared/public infrastructure (Railway), so this is the
# repo-root Dockerfile; squelchd deploys bare on the box (see deploy/DEPLOY.md).
#
# The build context must be the workspace root: squelch-relay is a workspace
# member and cargo needs the root manifest + lockfile to resolve it.

FROM rust:1-slim-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY squelch-core ./squelch-core
COPY squelch-httpauth ./squelch-httpauth
COPY squelch-mcp ./squelch-mcp
COPY squelch-tui ./squelch-tui
COPY squelch-api ./squelch-api
COPY squelchd ./squelchd
COPY squelch-relay ./squelch-relay

RUN cargo build --release --locked -p squelch-relay

FROM debian:bookworm-slim
# rustls everywhere, so the only runtime need is the CA roots for api.push.apple.com.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home relay

COPY --from=builder /build/target/release/squelch-relay /usr/local/bin/squelch-relay

# Mount point for the open buffer (SQUELCH_RELAY_DB_PATH=/data/opens.sqlite3).
# Created and owned here because the process is not root: a volume mounted over
# a root-owned path leaves the relay unable to create its SQLite file, and it
# refuses to boot rather than run with a buffer it cannot persist. Without a
# volume the relay still runs, holding opens in memory until the daemon drains.
RUN mkdir -p /data && chown relay /data
VOLUME ["/data"]

USER relay

# Railway injects PORT and terminates TLS at its edge; bind whatever it gives
# us on all interfaces unless the operator pinned SQUELCH_RELAY_BIND themselves.
CMD ["/bin/sh", "-c", "SQUELCH_RELAY_BIND=\"${SQUELCH_RELAY_BIND:-0.0.0.0:${PORT:-8850}}\" exec /usr/local/bin/squelch-relay"]
