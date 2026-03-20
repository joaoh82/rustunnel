# ── Stage 1: build dashboard UI ───────────────────────────────────────────────
FROM node:20-slim AS ui-builder

WORKDIR /ui

# Install dependencies first so this layer is cached independently of source.
COPY dashboard-ui/package*.json ./
RUN npm ci

COPY dashboard-ui/ ./

# `next build` with NEXT_EXPORT unset triggers `output: 'export'` in
# next.config.js and writes the static site to ./out/.
RUN npx next build

# ── Stage 2: build Rust server ────────────────────────────────────────────────
FROM rust:slim-bookworm AS builder

WORKDIR /build

# System deps needed by transitive crates (pkg-config, libssl for reqwest).
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first so dependency compilation is cached
# independently of source changes (layer-cache trick).
COPY Cargo.toml Cargo.lock ./
COPY crates/rustunnel-protocol/Cargo.toml crates/rustunnel-protocol/
COPY crates/rustunnel-server/Cargo.toml   crates/rustunnel-server/
COPY crates/rustunnel-client/Cargo.toml   crates/rustunnel-client/
COPY crates/rustunnel-mcp/Cargo.toml     crates/rustunnel-mcp/
COPY tests/Cargo.toml                    tests/

# Stub sources so cargo can resolve and compile all dependencies.
RUN mkdir -p \
        crates/rustunnel-protocol/src \
        crates/rustunnel-server/src \
        crates/rustunnel-client/src \
        crates/rustunnel-mcp/src \
        tests/integration && \
    printf '// stub' > tests/integration/stub.rs && \
    echo "pub fn _stub() {}" > crates/rustunnel-protocol/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/rustunnel-server/src/lib.rs  && \
    printf 'fn main() {}' > crates/rustunnel-server/src/main.rs    && \
    printf 'fn main() {}' > crates/rustunnel-client/src/main.rs    && \
    printf 'fn main() {}' > crates/rustunnel-mcp/src/main.rs

RUN cargo build --release -p rustunnel-server

# Copy the compiled dashboard assets from the UI build stage into the location
# that rust-embed picks up at compile time.
COPY --from=ui-builder /ui/out crates/rustunnel-server/src/dashboard/assets/

# Replace stubs with real source and rebuild (only changed crates recompile).
COPY crates/ crates/
RUN touch crates/rustunnel-protocol/src/lib.rs \
          crates/rustunnel-server/src/lib.rs \
          crates/rustunnel-server/src/main.rs \
          crates/rustunnel-client/src/main.rs && \
    cargo build --release -p rustunnel-server

# ── Stage 3: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root service account.
RUN useradd --system --no-create-home --shell /usr/sbin/nologin rustunnel

COPY --from=builder /build/target/release/rustunnel-server /usr/local/bin/rustunnel-server

# Config and state directories.
RUN mkdir -p /etc/rustunnel /var/lib/rustunnel && \
    chown rustunnel:rustunnel /var/lib/rustunnel

USER rustunnel

# HTTP edge   HTTPS edge   control-plane WS   dashboard   metrics
EXPOSE 80 443 4040 8443 9090

ENTRYPOINT ["/usr/local/bin/rustunnel-server"]
CMD ["--config", "/etc/rustunnel/server.toml"]
