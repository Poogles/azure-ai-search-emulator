FROM rust:1.95-slim-bookworm AS builder

# Build a static musl binary for the platform Docker is running on, so the
# image works on both amd64 (CI) and arm64 (Apple Silicon) hosts.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH}" in \
      amd64) TARGET="x86_64-unknown-linux-musl" ;; \
      arm64) TARGET="aarch64-unknown-linux-musl" ;; \
      *) echo "unsupported arch: ${TARGETARCH}"; exit 1 ;; \
    esac; \
    rustup target add "${TARGET}"; \
    apt-get update; \
    apt-get install -y --no-install-recommends musl-tools; \
    rm -rf /var/lib/apt/lists/*; \
    echo "${TARGET}" > /target.txt

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN set -eux; \
    TARGET="$(cat /target.txt)"; \
    cargo build --release --locked --target "${TARGET}"; \
    cp "/app/target/${TARGET}/release/aisearch-emulator" /app/aisearch-emulator

FROM gcr.io/distroless/static-debian12

COPY --from=builder /app/aisearch-emulator /aisearch-emulator

USER nonroot

ENV EMULATOR_PORT=8080
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/aisearch-emulator", "healthcheck"]

ENTRYPOINT ["/aisearch-emulator"]
