# dam-web 多阶段构建（W0）
#   stage1: node22 构建 web/ 前端产物
#   stage2: rust 构建 crates/app-web（release）
#   stage3: debian-slim 运行时（distroless 备选被否：便于排障/绑 volume）
# 构建代理统一 infer-host:11700（infer-host 本机 11700 已停用）；Gitea/内网走 NO_PROXY。
# 注意：build 容器内解析不了 LAN 主机名，代理必须写 IP。
# 构建：docker build -f deploy/Dockerfile --build-arg HTTP_PROXY=... -t dam-web:dev .

FROM node:22-slim AS web
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY=127.0.0.1,localhost,172.16.0.0/12,10.0.0.0/8,192.168.0.0/16
ENV npm_config_store_dir=/pnpm-store
WORKDIR /build
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml* web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile || pnpm install
COPY web/ ./
RUN pnpm build

FROM rust:1-slim-bookworm AS bin
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY=127.0.0.1,localhost,172.16.0.0/12,10.0.0.0/8,192.168.0.0/16
WORKDIR /build
# W2：core-media(libvips 缩略图/pHash) 需要 vips/glib 开发包
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config build-essential \
    libvips-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY registry/ registry/
# core-config 用 include_str! 把内置包的 YAML 编进二进制（packs/*/taxonomy|rules|pipeline|report.yaml）
COPY packs/ packs/
RUN cargo build --release -p app-web

FROM debian:bookworm-slim
ARG NO_PROXY=127.0.0.1,localhost,172.16.0.0/12,10.0.0.0/8,192.168.0.0/16
# W2：运行时 libvips 动态链接（缩略图按需生成/pHash）
RUN apt-get update && apt-get install -y --no-install-recommends libvips42 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=bin /build/target/release/app-web /usr/local/bin/app-web
COPY --from=web /build/dist /app/web/dist
ENV DAM_WEB_DATA=/data \
    DAM_WEB_DIST=/app/web/dist \
    DAM_WEB_BIND=0.0.0.0 \
    DAM_WEB_PORT=8787
VOLUME /data
EXPOSE 8787
CMD ["app-web"]
