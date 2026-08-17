# Builds squelchd — the self-hosted daemon (Gmail sync loop + agent door /mcp +
# human door /client). Build context must be the workspace root:
#
#   docker build -f squelchd/Dockerfile .
#
# Multi-arch: the builder always runs on the CI runner's native arch
# (BUILDPLATFORM) and cross-compiles when TARGETPLATFORM differs, so a plain
# amd64 runner produces the arm64 image without QEMU. The only cross needs are
# the Debian cross g++ (rusqlite compiles bundled SQLite; ort links a static
# onnxruntime that wants libstdc++) and the matching cargo linker env.

# Trixie, not bookworm: ort's prebuilt static onnxruntime is compiled against
# glibc >= 2.38 (__isoc23_strtol and friends); bookworm's 2.36 can't link it.
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS builder
ARG BUILDPLATFORM
ARG TARGETPLATFORM
WORKDIR /build

RUN set -eux; \
    case "$TARGETPLATFORM" in \
      linux/amd64) echo x86_64-unknown-linux-gnu > /rust-triple ;; \
      linux/arm64) echo aarch64-unknown-linux-gnu > /rust-triple ;; \
      *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    triple="$(cat /rust-triple)"; \
    # ort links -lstdc++ even natively, and rust:slim ships gcc without g++;
    # the cross package additionally brings the target-arch libstdc++.
    pkgs="g++"; \
    if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ]; then \
      prefix="$(echo "$triple" | sed 's/-unknown//')"; \
      pkgs="$pkgs g++-$(echo "$prefix" | tr '_' '-')"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $pkgs; \
    rm -rf /var/lib/apt/lists/*; \
    rustup target add "$triple"

COPY Cargo.toml Cargo.lock ./
COPY squelch-core ./squelch-core
COPY squelch-httpauth ./squelch-httpauth
COPY squelch-ratelimit ./squelch-ratelimit
COPY squelch-mcp ./squelch-mcp
COPY squelch-tui ./squelch-tui
COPY squelch-api ./squelch-api
COPY squelch-relay ./squelch-relay
COPY squelch-broker ./squelch-broker
# Hosted control plane + VPS provisioner. Neither ships in THIS image, but both
# are workspace members and cargo refuses to resolve the workspace without them.
COPY squelch-control ./squelch-control
COPY squelch-warden ./squelch-warden
COPY squelchd ./squelchd

RUN set -eux; \
    triple="$(cat /rust-triple)"; \
    if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ]; then \
      prefix="$(echo "$triple" | sed 's/-unknown//')"; \
      lower="$(echo "$triple" | tr '-' '_')"; \
      upper="$(echo "$lower" | tr '[:lower:]' '[:upper:]')"; \
      export "CARGO_TARGET_${upper}_LINKER=${prefix}-gcc" \
             "CC_${lower}=${prefix}-gcc" \
             "CXX_${lower}=${prefix}-g++"; \
    fi; \
    cargo build --release --locked -p squelchd --target "$triple"; \
    cp "target/$triple/release/squelchd" /squelchd

FROM debian:trixie-slim
# rustls everywhere — the only runtime need is CA roots for Google's APIs (and
# the one-time fastembed weights download). Trixie to match the builder's glibc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home-dir /data --no-create-home squelch

COPY --from=builder /squelchd /usr/local/bin/squelchd

# All state lives under one mount: sqlite db, file-backend credentials, and
# (via HOME) the fastembed ONNX weights cache at /data/.local/share/squelch/models.
# Owned here for the no-volume case; a mounted volume arrives root-owned and
# overlays this, which the entrypoint fixes before dropping privileges.
RUN mkdir -p /data && chown squelch:squelch /data

# In a container, loopback-only would make the published port unreachable —
# bind all interfaces and let the operator control exposure with -p / a proxy.
# Everything else matches deploy/DEPLOY.md; see deploy/DOCKER.md for the
# required vars (SQUELCH_API_TOKEN, SQUELCH_ACCOUNT_EMAIL, SQUELCH_CLIENT_ID/
# _SECRET, and SQUELCH_MCP_ALLOWED_HOSTS when fronted by a proxy).
ENV SQUELCH_BIND=0.0.0.0:8848 \
    SQUELCH_CRED_BACKEND=file \
    SQUELCH_DB_PATH=/data/squelch.db \
    SQUELCH_CREDENTIALS_PATH=/data/credentials.json \
    HOME=/data

EXPOSE 8848

COPY squelchd/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Still root here: the entrypoint chowns the mounted volume, then execs
# squelchd as `squelch` via setpriv. The daemon itself never runs privileged.
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
