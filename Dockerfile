# Stage 1: Build Rust server
FROM rust:latest AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y clang libclang-dev pkg-config && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p loomem-server -p loomem-migrate

# Stage 2: Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates libstdc++6 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/loomem-server ./
COPY --from=builder /app/target/release/loomem-migrate ./
COPY scripts/docker-entrypoint.sh ./
RUN chmod +x docker-entrypoint.sh
COPY config.toml ./
COPY entities.cloud.toml ./entities.toml
# Cloud overrides: bind 0.0.0.0, data on volume, rate limiting on
# (audit 2026-07-01 item 3 — the sed range keeps the replacement scoped to
# the [rate_limit] section).
RUN sed -i 's/host = "127.0.0.1"/host = "0.0.0.0"/' config.toml && \
    sed -i 's|data_dir = "./data"|data_dir = "/data"|' config.toml && \
    sed -i '/^\[rate_limit\]/,/^\[/ s/^enabled = false/enabled = true/' config.toml && \
    mkdir -p /data
# Networked-profile fail-safes (audit 2026-07-01 items 1+2): the image binds
# 0.0.0.0, so require an explicit at-rest master key — the server refuses to
# start without LOOMEM_AT_REST_MASTER_KEY unless the operator opts out with
# LOOMEM_AT_REST_EXPECT_ENABLED=0. Auth is enforced in code: without
# LOOMEM_AUTH_TOKEN the server refuses a non-loopback bind
# (LOOMEM_ALLOW_UNAUTH=1 to override deliberately).
ENV LOOMEM_AT_REST_EXPECT_ENABLED=1
EXPOSE 3030
CMD ["./docker-entrypoint.sh"]
