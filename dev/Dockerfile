# dam-desktop dev container
# Debian bookworm-slim + rustup(stable/minimal) + node22 + pnpm + Tauri 2 (webkit2gtk-4.1) toolchain
# Build uses host network so 127.0.0.1:11700 (host V2Ray proxy) is reachable via build args.

FROM debian:bookworm-slim

# proxy only as build-time ARGs (NOT baked as ENV: 127.0.0.1 is wrong inside the container)
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY=127.0.0.1,localhost,172.16.0.0/12,10.0.0.0/8,192.168.0.0/16

ENV DEBIAN_FRONTEND=noninteractive

# rust: CARGO_HOME=/cargo is shadowed by named volume dam_cargo_registry
# (named volumes copy image content on first use, so rustup shims survive)
ENV RUSTUP_HOME=/rustup \
    CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/cargo-target \
    PATH=/cargo/bin:$PATH \
    npm_config_store_dir=/pnpm-store

# system build deps (Tauri 2 on Linux) + xvfb/xauth for headless GUI tests
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git file pkg-config build-essential \
        libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev libsoup-3.0-dev \
        libjavascriptcoregtk-4.1-dev libxdo-dev libssl-dev \
        libayatana-appindicator3-dev librsvg2-dev libvips-dev \
        xvfb xauth x11-utils \
    && rm -rf /var/lib/apt/lists/*

# rust stable (minimal profile)
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

# node 22 (NodeSource) + pnpm
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g pnpm@latest

WORKDIR /workspace

CMD ["sleep", "infinity"]
