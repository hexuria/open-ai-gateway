# syntax=docker/dockerfile:1

# Build on the build platform and cross-compile, rather than emulating the
# target under QEMU — a Rust build under QEMU is punishing.
FROM --platform=$BUILDPLATFORM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

# Dependencies first, so a source-only change does not rebuild the world.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin oag && \
    cp target/release/oag /oag

FROM alpine:3.21
RUN apk add --no-cache ca-certificates tzdata curl && \
    adduser -D -u 1000 oag
COPY --from=builder /oag /usr/local/bin/oag
COPY migrations /app/migrations
USER oag
WORKDIR /app

# 8080 inference (the load balancer fronts this), 8081 admin (it does not).
EXPOSE 8080 8081

# Readiness, not liveness: this checks Postgres and Redis are actually
# reachable. A container whose database connection has died still answers a
# liveness probe perfectly well, which is how you end up balancing onto it.
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8081/health/ready || exit 1

ENTRYPOINT ["oag"]
CMD ["serve"]
