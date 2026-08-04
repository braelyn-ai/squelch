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
COPY squelch-broker ./squelch-broker
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
# Owned here for the no-volume case; when a volume IS mounted it arrives owned
# by root and overlays this, which is what the entrypoint fixes before dropping
# privileges. Without a volume the relay still runs, holding opens in memory
# until the daemon drains them.
RUN mkdir -p /data && chown relay:relay /data

COPY squelch-relay/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Deliberately still root at this point: the entrypoint chowns the mounted
# volume and then execs the relay as `relay` via setpriv. The server itself
# never runs privileged.
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
