# syntax=docker/dockerfile:1
# tiny-eye: one binary with the web UI embedded, plus the pinned DuckDB extensions it loads.
#   docker build -t tiny-eye .
#   docker run -p 7070:7070 -v tiny-eye-data:/data tiny-eye
# Configuration: TINY_EYE__SECTION__KEY environment variables (see config/default.yaml), or mount
# a file and pass `--config /path/file.yaml`.

FROM node:20-bookworm-slim AS ui
WORKDIR /src
COPY api ./api
COPY ui/package.json ui/package-lock.json ./ui/
RUN cd ui && npm ci --no-audit --no-fund
COPY ui ./ui
RUN cd ui && npm run build

FROM rust:1-bookworm AS build
# DuckDB's bundled C++ build is memory hungry: cap parallel jobs (raise on big builders) and skip
# debug info in the image build.
ARG BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${BUILD_JOBS} CARGO_PROFILE_RELEASE_DEBUG=false
WORKDIR /src
# The exact Rust version comes from rust-toolchain.toml; install it before the sources so this
# layer stays cached until the version is bumped.
COPY rust-toolchain.toml .
RUN rustup install
COPY . .
COPY --from=ui /src/ui/dist ./ui/dist
# One target cache per architecture (amd64 and arm64 build in parallel). Touching build.rs makes
# it run again, so the UI just copied in is embedded even when the cached target looks fresh.
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=tiny-eye-target-${TARGETARCH} \
    touch crates/server/build.rs && \
    cargo build --release --bin tiny-eye && cp target/release/tiny-eye /tiny-eye

# DuckDB extensions pinned to the bundled DuckDB version (D12: shipped, never auto-installed).
FROM debian:bookworm-slim AS ext
ARG DUCKDB_VERSION=v2.0.1
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN plat="linux_$(dpkg --print-architecture)" && mkdir -p "/ext/${DUCKDB_VERSION}/${plat}" && \
    for e in ducklake httpfs icu postgres_scanner; do \
      curl -sSfL "http://extensions.duckdb.org/${DUCKDB_VERSION}/${plat}/${e}.duckdb_extension.gz" | gunzip > "/ext/${DUCKDB_VERSION}/${plat}/${e}.duckdb_extension"; \
    done

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data tinyeye && mkdir -p /data /etc/tiny-eye && chown tinyeye /data
COPY --from=build /tiny-eye /usr/local/bin/tiny-eye
COPY --from=ext /ext /opt/tiny-eye/duckdb_ext
COPY --chown=tinyeye dashboards /etc/tiny-eye/dashboards
ENV TINY_EYE__SERVER__LISTEN=0.0.0.0:7070 \
    TINY_EYE__DUCKDB__EXTENSION_DIR=/opt/tiny-eye/duckdb_ext \
    TINY_EYE__STORAGE__URL=/data/objects \
    TINY_EYE__CATALOG__URL=/data/catalog.ducklake \
    TINY_EYE__INGEST__WAL_DIR=/data/wal \
    TINY_EYE__MATERIALIZE__PATH=/data/materialized.duckdb \
    TINY_EYE__DASHBOARDS__DIR=/etc/tiny-eye/dashboards
USER tinyeye
WORKDIR /data
VOLUME /data
EXPOSE 7070
ENTRYPOINT ["tiny-eye"]
