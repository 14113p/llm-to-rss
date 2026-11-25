# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY tests ./tests

RUN cargo build --release --locked 2>/dev/null || cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/sh appuser
WORKDIR /app

COPY --from=builder /app/target/release/llm-to-rss /usr/local/bin/llm-to-rss

USER appuser
EXPOSE 8080

ENTRYPOINT ["llm-to-rss"]
CMD ["serve", "--config", "/config/config.yaml"]
