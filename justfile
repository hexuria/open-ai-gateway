# One-command workflows. `just` with no target lists them.

default:
    @just --list

compose := "docker compose -f deploy/compose/dev.yml"
stack   := "docker compose -f deploy/compose/stack.yml"
dev_db  := "postgres://oag:oag@127.0.0.1:5452/oag"
dev_rd  := "redis://127.0.0.1:6399"

# ── the gate ───────────────────────────────────────────────────────────────────
# What CI runs, ordered so the cheapest check fails first.
check: fmt-check lint test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# ── dev loop ───────────────────────────────────────────────────────────────────
# Infrastructure only; the gateway runs on the host so rebuilds stay fast.
dev-up:
    {{compose}} up -d --wait

dev-down:
    {{compose}} down

# Stop and delete the data. The one destructive target.
dev-reset:
    {{compose}} down -v

# Bring up infrastructure and migrate it.
dev: dev-up migrate

migrate:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run --quiet -p oag -- migrate

# Run the gateway against the dev infrastructure.
serve:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SERVER__PUBLIC_ADDR="127.0.0.1:8080" \
      OAG_SERVER__ADMIN_ADDR="127.0.0.1:8081" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run -p oag -- serve

# Show the resolved config, secrets redacted.
config:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run --quiet -p oag -- config

# ── the full topology ──────────────────────────────────────────────────────────
# Caddy -> Envoy -> 3 replicas -> Postgres + Redis. The full topology.
stack-up:
    OAG_SIGNING_SECRET="$(just _dev-secret)" \
    OAG_CREDENTIAL_KEK="$(just _dev-kek)" \
      {{stack}} up -d --wait --build

stack-down:
    {{stack}} down

stack-logs:
    {{stack}} logs -f

# Restart one replica under load; the other two must not drop a stream.
stack-roll:
    {{stack}} restart oag-2

# ── helpers ────────────────────────────────────────────────────────────────────
# Deterministic dev-only secrets. Never use these anywhere real.
_dev-secret:
    @echo "dev-only-signing-secret-do-not-use-in-production-0001"

_dev-kek:
    @echo "b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA="
