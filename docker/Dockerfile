# syntax=docker/dockerfile:1.7
# DESIGN §18.1. Kept fully TARGETARCH/TARGETPLATFORM-parametrised, but CI builds
# linux/amd64 only (BRIEF §16) — arm64 is a one-line workflow change later.
ARG RUST_VERSION=1.95
# The builder stays on bookworm: its glibc is older than the runtime's, so the binary it produces
# runs on both. The runtime is trixie because the pinned `bgutil-pot` release links against
# GLIBC_2.38, which bookworm (2.36) does not have — the legacy image was trixie-based too, via
# `python:3.13-slim`.
ARG BUILDER_DEBIAN=bookworm
ARG RUNTIME_DEBIAN=trixie

# ---------- builder (cargo-chef for a cached dependency layer) ----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-${BUILDER_DEBIAN} AS chef
# cmake/clang/perl are needed by the vendored BoringSSL behind `wreq` (the SC
# Chrome-impersonating client, feature `sc-impersonate`); pkg-config and libclang by its bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake clang libclang-dev pkg-config perl \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETPLATFORM
ARG BUILDPLATFORM
# Resolve the rustc target, and install a cross linker only for a genuine cross build: on a native
# build the `gcc-<arch>-linux-gnu` package is wasted time and is not always available for the
# host's own architecture. /cargo-env carries the linker override for the cross case and is empty
# otherwise, so both cargo invocations below can source it unconditionally.
RUN set -eux; case "$TARGETPLATFORM" in \
      linux/amd64) T=x86_64-unknown-linux-gnu;  CC_PKG=gcc-x86-64-linux-gnu;  CXX_PKG=g++-x86-64-linux-gnu;  TRIPLE=x86_64-linux-gnu ;; \
      linux/arm64) T=aarch64-unknown-linux-gnu; CC_PKG=gcc-aarch64-linux-gnu; CXX_PKG=g++-aarch64-linux-gnu; TRIPLE=aarch64-linux-gnu ;; \
      *) echo "unsupported $TARGETPLATFORM" >&2; exit 1 ;; esac; \
    echo "$T" > /target.txt; rustup target add "$T"; \
    : > /cargo-env; \
    if [ "$TARGETPLATFORM" != "${BUILDPLATFORM:-$TARGETPLATFORM}" ]; then \
      apt-get update && apt-get install -y --no-install-recommends "$CC_PKG" "$CXX_PKG" \
        && rm -rf /var/lib/apt/lists/*; \
      V="$(echo "$T" | tr 'a-z-' 'A-Z_')"; \
      { echo "CARGO_TARGET_${V}_LINKER=${TRIPLE}-gcc"; \
        echo "CC_${T}=${TRIPLE}-gcc"; \
        echo "CXX_${T}=${TRIPLE}-g++"; } > /cargo-env; \
    fi
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    set -eux; set -a; . /cargo-env; set +a; \
    cargo chef cook --release --target "$(cat /target.txt)" --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    set -eux; set -a; . /cargo-env; set +a; \
    cargo build --release --locked --target "$(cat /target.txt)" -p aulos-server; \
    cp "target/$(cat /target.txt)/release/aulos-server" /aulos-server

# ---------- runtime ----------
FROM debian:${RUNTIME_DEBIAN}-slim
ARG TARGETARCH
ARG YTDLP_VERSION=2026.8.30.232658.dev0
ARG BGUTIL_TAG=v0.8.1
ARG NM3U8DL_VERSION=v0.5.1-beta
ARG NM3U8DL_BUILD=20251029
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl unzip file tini gosu \
      ffmpeg aria2 coreutils python3 python3-pip libssl3t64 libstdc++6 \
 && rm -rf /var/lib/apt/lists/* && mkdir -p /.cache && chmod 777 /.cache

# yt-dlp: the nightly pin, exactly as the legacy image did.
# The pin lives in exactly ONE place in the repo (this ARG) — update-yt-dlp.yml asserts that.
RUN pip3 install --break-system-packages --no-cache-dir --no-deps "yt-dlp==${YTDLP_VERSION}"

# deno (yt-dlp[deno] / yt-dlp-ejs JS challenge solver)
RUN curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/usr/local sh -s -- -y

# BgUtils POT: sidecar binary + yt-dlp plugin into site-packages
RUN set -eux; case "$TARGETARCH" in amd64) A=x86_64 ;; arm64) A=aarch64 ;; \
      *) echo "unsupported $TARGETARCH" >&2; exit 1 ;; esac; \
    B=https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/${BGUTIL_TAG}; \
    curl -fL -o /usr/local/bin/bgutil-pot "$B/bgutil-pot-linux-${A}"; \
    chmod +x /usr/local/bin/bgutil-pot; \
    PD="$(python3 -c 'import site; print(site.getsitepackages()[0])')"; \
    curl -fL -o /tmp/p.zip "$B/bgutil-ytdlp-pot-provider-rs.zip"; \
    unzip -oq /tmp/p.zip -d "$PD"; rm /tmp/p.zip

# N_m3u8DL-RE (StreamingCommunity HLS)
RUN set -eux; A=$([ "$TARGETARCH" = "arm64" ] && echo arm64 || echo x64); \
    curl -fL "https://github.com/nilaoda/N_m3u8DL-RE/releases/download/${NM3U8DL_VERSION}/N_m3u8DL-RE_${NM3U8DL_VERSION}_linux-${A}_${NM3U8DL_BUILD}.tar.gz" \
      -o /tmp/n.tgz && tar -xzf /tmp/n.tgz -C /usr/local/bin \
      && chmod +x /usr/local/bin/N_m3u8DL-RE && rm /tmp/n.tgz

COPY --from=builder /aulos-server /usr/local/bin/aulos-server
COPY crates/aulos-provider-ytdlp/python/ /app/python/
COPY docker/entrypoint.sh /usr/local/bin/aulos-entrypoint
RUN sed -i 's/\r$//' /usr/local/bin/aulos-entrypoint && chmod +x /usr/local/bin/aulos-entrypoint
COPY plugins/examples /opt/aulos/plugins-examples

ENV PUID=1000 PGID=1000 UMASK=022 \
    DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube TEMP_DIR=/downloads \
    PORT=8081 SC_USE_FFMPEG=false \
    DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1 \
    AULOS_PLUGINS_DIR=/config/plugins \
    PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 RUST_BACKTRACE=1
VOLUME /downloads
EXPOSE 8081
# NOT a shell `curl` interpolating ${URL_PREFIX}: that bypasses the §17.1 normalisation and yields
# `http://127.0.0.1:8081metubehealthz` for URL_PREFIX=metube (C16). The subcommand loads the same
# Config and builds the URL through the same Prefix newtype.
HEALTHCHECK --interval=30s --timeout=5s --start-period=25s --retries=3 \
  CMD ["/usr/local/bin/aulos-server","healthcheck"]
ARG VERSION=dev
ENV METUBE_VERSION=$VERSION AULOS_VERSION=$VERSION
ENTRYPOINT ["/usr/bin/tini","-g","--","/usr/local/bin/aulos-entrypoint"]
CMD ["serve"]
