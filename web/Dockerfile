# syntax=docker/dockerfile:1
# Sol in a browser on your own server: the cores built to WebAssembly, the page built by Vite, both served
# by Caddy. The cores are the workspace's, so the build runs from the repository's root:
#
#   docker build -f web/Dockerfile -t sol-web .        # or: cd web && docker compose up -d
#
# Build arguments: SOL_BASE, the path the page is served under (/ by default, e.g. /sol/); SOL_COMMIT,
# the commit Settings › System shows (git rev-parse --short HEAD).

# The cores, with the toolchain the wasm-against-native check ran on (docs/web.md).
FROM rust:1.96.0-slim AS core
RUN rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY cores cores
COPY shared shared
COPY web/core web/core
RUN cargo build --locked --release --target wasm32-unknown-unknown -p solweb

# The page.
FROM node:24-slim AS page
RUN npm install --global pnpm@10.32.1
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY --from=core /src/target/wasm32-unknown-unknown/release/solweb.wasm /src/target/wasm32-unknown-unknown/release/solweb.wasm
ARG SOL_BASE=/
ARG SOL_COMMIT=
RUN SOL_BASE="$SOL_BASE" SOL_COMMIT="$SOL_COMMIT" pnpm build

# The server: static files, nothing else.
FROM caddy:2-alpine
ARG SOL_BASE=/
COPY web/Caddyfile /etc/caddy/Caddyfile
COPY --from=page /src/web/dist /srv${SOL_BASE}
EXPOSE 80 443
