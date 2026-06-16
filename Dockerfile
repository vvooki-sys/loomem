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
# Cloud overrides: bind 0.0.0.0, data on volume
RUN sed -i 's/host = "127.0.0.1"/host = "0.0.0.0"/' config.toml && \
    sed -i 's|data_dir = "./data"|data_dir = "/data"|' config.toml && \
    mkdir -p /data
EXPOSE 3030
CMD ["./docker-entrypoint.sh"]
