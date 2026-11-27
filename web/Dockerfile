# syntax=docker/dockerfile:1
# Sol in a browser on your own server: the cores built to WebAssembly, the page built by Vite, both served
# by Caddy. The cores are the workspace's, so the build runs from the repository's root:
#
#   docker build -f web/Dockerfile -t sol-web .        # or: cd web && docker compose up -d
#
# Build arguments: SOL_BASE, the path the page is served under (/ by default, e.g. /sol/); SOL_COMMIT,
# the commit Settings › System shows (git rev-parse --short HEAD).

# The cores, with the toolchain the wasm-against-native check ran on (docs/web.md).
FROM rust:1.96.0-slim AS sources
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY cores cores
COPY shared shared
COPY web/core web/core
COPY netplay netplay

FROM sources AS rooms-build
RUN cargo build --locked --release -p sol-rooms

FROM debian:trixie-slim AS rooms
COPY --from=rooms-build /src/target/release/sol-rooms /usr/local/bin/
ENV SOL_ROOMS_LISTEN=0.0.0.0:8788
USER nobody
EXPOSE 8788
ENTRYPOINT ["sol-rooms"]

FROM sources AS core
RUN rustup target add wasm32-unknown-unknown
RUN cargo build --locked --release --target wasm32-unknown-unknown -p solweb -p sol-netplay

# The page.
FROM node:24-slim AS page
RUN npm install --global pnpm@10.32.1
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY --from=core /src/target/wasm32-unknown-unknown/release/*.wasm /src/target/wasm32-unknown-unknown/release/
ARG SOL_BASE=/
ARG SOL_COMMIT=
RUN SOL_BASE="$SOL_BASE" SOL_COMMIT="$SOL_COMMIT" pnpm build

# The server: static files, nothing else.
FROM caddy:2-alpine
ARG SOL_BASE=/
COPY web/Caddyfile /etc/caddy/Caddyfile
COPY --from=page /src/web/dist /srv${SOL_BASE}
EXPOSE 80 443
