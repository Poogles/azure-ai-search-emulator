FROM rust:1.85-slim-bookworm AS builder

RUN rustup target add x86_64-unknown-linux-musl && \
    apt-get update && \
    apt-get install -y --no-install-recommends musl-tools && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM gcr.io/distroless/static-debian12

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/aisearch-emulator /aisearch-emulator

ENV EMULATOR_PORT=8080
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/aisearch-emulator", "healthcheck"]

ENTRYPOINT ["/aisearch-emulator"]
