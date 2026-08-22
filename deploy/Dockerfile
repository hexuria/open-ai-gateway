# syntax=docker/dockerfile:1

# Deliberately *not* pinned to $BUILDPLATFORM. Pinning the builder to the build
# platform without also passing --target to cargo produces a binary for the
# build arch inside an image tagged for the target arch — an arm64 manifest
# holding an amd64 binary, which fails at exec time with a bad-format error.
# Each architecture is instead built natively on its own runner and the two are
# merged into one manifest list (see .github/workflows/release.yml), so there is
# no cross-compilation and no QEMU, both of which are painful for Rust.
FROM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml clippy.toml ./
COPY crates ./crates
# Migrations are needed at *compile* time, not just at run time: `sqlx::migrate!`
# embeds them into the binary. Copying them only into the runtime stage makes
# the build fail with a confusing "canonicalizing migration directory" error.
COPY migrations ./migrations
# The dashboard is embedded with include_str!, so it is a build-time input for
# the same reason the migrations are.
COPY web ./web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin oag && \
    cp target/release/oag /oag

FROM alpine:3.21
RUN apk add --no-cache ca-certificates tzdata curl && \
    adduser -D -u 1000 oag
COPY --from=builder /oag /usr/local/bin/oag
# No migrations copied here on purpose: they are compiled into the binary, so a
# second copy on disk could only ever disagree with the one that runs.
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
