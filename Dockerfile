# syntax=docker/dockerfile:1

# --- Build stage ---
# Full (non-slim) rust image: provides cc for crates like ring/aws-lc-rs.
FROM rust:1.98-bookworm AS builder
WORKDIR /app

# Compile dependencies first with a dummy entrypoint so this layer is
# cached as long as Cargo.toml/Cargo.lock are unchanged.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# touch sources: COPY-normalized mtimes can look older than the dummy
# build artifacts, which would ship the stub binary instead of ours.
COPY src ./src
RUN touch src/*.rs && cargo build --release

# --- Runtime stage ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 stac

COPY --from=builder /app/target/release/stac-os-rs /usr/local/bin/

USER stac
EXPOSE 3000
ENV OPENSEARCH_URL=http://opensearch:9200
CMD ["stac-os-rs"]
