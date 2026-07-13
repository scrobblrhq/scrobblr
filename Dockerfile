# syntax=docker/dockerfile:1

# Builds both backend binaries (api + worker) into one small runtime image.
# Compose runs the same image twice with different commands.

##############################
# Builder
##############################
FROM rust:1-bookworm AS builder

# sqlx (runtime-tokio-native-tls) and reqwest (native-tls) link OpenSSL,
# so the builder needs the headers.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# sqlx query macros compile against the committed .sqlx/ cache — no DB needed.
ENV SQLX_OFFLINE=true

COPY . .

# BuildKit cache mounts keep the cargo registry and target dir warm across
# builds; the binaries are copied out of the (cache-mounted) target dir in the
# same layer so they survive.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p api -p worker \
    && cp target/release/api /usr/local/bin/api \
    && cp target/release/worker /usr/local/bin/worker

##############################
# Runtime
##############################
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home scrobblr

COPY --from=builder /usr/local/bin/api /usr/local/bin/api
COPY --from=builder /usr/local/bin/worker /usr/local/bin/worker

# Uploaded images land here; mount a volume to persist them.
ENV UPLOAD_DIR=/data/uploads
RUN mkdir -p /data/uploads && chown -R scrobblr:scrobblr /data

USER scrobblr
WORKDIR /data
EXPOSE 8080

# Default to the API; the worker service overrides this in compose.
CMD ["api"]
