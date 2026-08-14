FROM rust:1.88-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends cmake build-essential && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml ./
COPY apps ./apps
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --workspace

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 wget && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/prompt-api /usr/local/bin/prompt-api
COPY --from=builder /app/target/release/prompt-analyzer /usr/local/bin/prompt-analyzer
USER 65532:65532
