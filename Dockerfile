# syntax=docker/dockerfile:1
#
# sendme-balloon — multi-stage container image
#
# The container packages the `sendme` CLI (send / receive files over iroh
# with NAT hole-punching).  The `sendme-balloon` desktop GUI is not included
# because it requires a display server; it is distributed as a direct binary
# download via GitHub Releases instead.
#
# Build:
#   docker build -t ghcr.io/imp1sh/sendme-balloon .
#
# Run:
#   docker run --rm ghcr.io/imp1sh/sendme-balloon --help
#   docker run --rm -v "$PWD:/data" ghcr.io/imp1sh/sendme-balloon send /data/myfile

# ── Builder ────────────────────────────────────────────────────────────────
FROM rust:1.91-bookworm AS builder

WORKDIR /build

# Copy manifests first for layer caching of dependency compilation.
COPY Cargo.toml Cargo.lock ./

# Copy source tree.
COPY src/ src/

# Compile the release binary.  BuildKit cache mounts avoid re-downloading
# crates and re-compiling unchanged dependencies across builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin sendme && \
    cp target/release/sendme /sendme

# ── Runtime ────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# OCI standard labels — populated by CI via --label overrides when available.
LABEL org.opencontainers.image.title="sendme-balloon" \
      org.opencontainers.image.description="A cli tool to send directories over the network, with NAT hole punching" \
      org.opencontainers.image.source="https://github.com/imp1sh/sendme-balloon" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"

# Minimal runtime: CA certificates for TLS (relay connections), nothing else.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --no-create-home --shell /usr/sbin/nologin sendme

COPY --from=builder /sendme /usr/local/bin/sendme

USER sendme

ENTRYPOINT ["sendme"]
CMD ["--help"]
