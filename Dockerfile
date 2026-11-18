# syntax=docker/dockerfile:1

# ---- build ----------------------------------------------------------------
FROM rust:1.96-trixie AS build
# BoringSSL (btls-sys) and aws-lc need cmake and clang to compile.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang libclang-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ARG FEATURES=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features "$FEATURES" \
 && cp target/release/localleads /usr/local/bin/localleads

# ---- run ------------------------------------------------------------------
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin localleads \
 && mkdir -p /data && chown localleads:localleads /data
COPY --from=build /usr/local/bin/localleads /usr/local/bin/localleads

ENV LOCALLEADS_DB=/data/localleads.sqlite \
    LOCALLEADS_BIND=0.0.0.0:7878
VOLUME ["/data"]
EXPOSE 7878
USER localleads
WORKDIR /data
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["localleads", "healthcheck"]
ENTRYPOINT ["tini", "--", "localleads"]

LABEL org.opencontainers.image.title="LocalLeads" \
      org.opencontainers.image.description="Self-hosted Google Maps lead finder with email enrichment and an MCP server" \
      org.opencontainers.image.licenses="MIT"
