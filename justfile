# One-command workflows. `just` with no target lists them.

default:
    @just --list

compose := "docker compose -f deploy/compose/dev.yml"
stack   := "docker compose -f deploy/compose/stack.yml"
floci   := "docker compose -f deploy/floci/docker-compose.yml"
dev_db  := env("OAG_DATABASE__URL", "postgres://oag:oag@127.0.0.1:5452/oag")
dev_rd  := env("OAG_REDIS__URL", "redis://127.0.0.1:6399")

# Local dev ports. Deliberately not 8080/8081, which collide with roughly every
# other dev server, and deliberately in the 1024–32768 band: macOS hands out
# 49152+ for outbound sockets and Linux 32768+, so anything above those can be
# taken by the kernel out from under you between a check and a bind. These are
# only the *host* ports for `just serve`; containers still listen on 8080/8081
# internally, where nothing can collide.
#
# `serve` walks upward from these to the first genuinely free pair, so a clash
# shifts the port instead of failing. Override the starting point if you want a
# fixed one: `just pub_port=31000 adm_port=31001 serve`.
pub_port := "29080"
adm_port := "29081"

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

# What the editor's run button launches (.claude/launch.json), so it must be safe
# from a cold machine: `dev` is idempotent, and `serve` stays in the foreground,
# which is what a dev-server supervisor expects.
# Infrastructure, migrations, then the gateway — one target.
dev-serve: dev
    @just serve

migrate:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run --quiet -p oag -- migrate

# A route, a principal, an API key, and the model catalog. Prints the key.
bootstrap:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      sh -c 'cargo run --quiet -p oag -- admin init --email dev@localhost && \
             cargo run --quiet -p oag -- admin seed-catalog'

# Mint an inference key — for sending requests to :29080. `just key` or
# `just key name=codex`. This is the key that goes in an SDK / client config.
key name="cli":
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run --quiet -p oag -- admin key --email dev@localhost --name {{name}}

# Mint an admin key — for the dashboard and admin API on :29081. `just admin-key`.
# Deliberately separate from an inference key: an SDK key must not reach admin.
admin-key name="admin":
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run --quiet -p oag -- admin key --email dev@localhost --name {{name}} --admin

# Run the gateway against the dev infrastructure.
serve:
    @set -- $(just _free-ports); pub=$1; adm=$2; \
      if [ "$pub" != "{{pub_port}}" ] || [ "$adm" != "{{adm_port}}" ]; then \
        echo "port {{pub_port}}/{{adm_port}} taken — using $pub/$adm instead"; \
      fi; \
      echo "  inference  http://127.0.0.1:$pub"; \
      echo "  dashboard  http://127.0.0.1:$adm"; \
      OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SERVER__PUBLIC_ADDR="127.0.0.1:$pub" \
      OAG_SERVER__ADMIN_ADDR="127.0.0.1:$adm" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      cargo run -p oag -- serve

# The pair `serve` would use right now.
ports:
    @set -- $(just _free-ports); \
      echo "inference  127.0.0.1:$1"; \
      echo "dashboard  127.0.0.1:$2"

# First free pair at or above the configured start.
#
# Binding is the only reliable test: `lsof` misses sockets held in another
# network namespace, and parsing it races anything starting concurrently. This
# still races, but only across the gap between here and the server's own bind.
_free-ports:
    #!/usr/bin/env python3
    import socket

    def free(port, taken):
        while port < 32768:
            if port not in taken:
                probe = socket.socket()
                try:
                    probe.bind(("127.0.0.1", port))
                    probe.close()
                    return port
                except OSError:
                    pass
            port += 1
        raise SystemExit("no free port below 32768")

    inference = free({{pub_port}}, set())
    dashboard = free({{adm_port}}, {inference})
    print(inference, dashboard)

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

# The same stack plus Prometheus and Grafana (:9090, :3000).
stack-observe:
    OAG_SIGNING_SECRET="$(just _dev-secret)" \
    OAG_CREDENTIAL_KEK="$(just _dev-kek)" \
      {{stack}} --profile observability up -d --wait --build

# Restart one replica under load; the other two must not drop a stream.
stack-roll:
    {{stack}} restart oag-2

# ── local "GCP" via floci ────────────────────────────────────────────────────
# Deploy OAG onto a local floci GCP as a real Cloud Run service. See deploy/floci.
floci-up:
    @./deploy/floci/deploy.sh

# Tear down floci + Postgres + Redis, and the Cloud Run container floci spawned.
floci-down:
    -@docker ps -aq --filter "name=floci-gcp-cloudrun-open-ai-gateway" | xargs -r docker rm -f
    {{floci}} down

# Follow the floci emulator + gateway logs.
floci-logs:
    {{floci}} logs -f

# ── verification ───────────────────────────────────────────────────────────────
# Needs no credentials and no cluster, and guards the savings figure itself.
# The request path end to end against a mock upstream, in about a minute.
verify:
    @./deploy/test/local-verify.sh

# Needs kind, takes several minutes. Passes in CI (`.github/workflows/k8s.yml`)
# on every relevant push: 8 of 8 streams across a rollout restart, all 8 in the
# ledger. See the status note at the top of deploy/test/kind-verify.sh. Proves
# drain, not circuit breakers.
# A rolling restart severing no live stream, and every drained stream metered.
verify-k8s:
    @./deploy/test/kind-verify.sh

# Circuit breaker end to end against the Python mock. Two 408s trip it; the
# third request is refused without another upstream call. No credentials.
verify-breakers:
    @./deploy/test/breaker-verify.sh

# OpenAI and Gemini adapters against aimock. Needs Node (`npx`). Dummy keys.
verify-dialects:
    @./deploy/test/dialects-verify.sh

# Bedrock event-stream against VidaiMock's independent encoder. Needs Docker.
verify-bedrock:
    @./deploy/test/bedrock-verify.sh

# OpenAI Chat Completions client over the Anthropic Python mock. The hub, not
# a native adapter: `just verify` is Anthropic-to-Anthropic and `verify-dialects`
# is OpenAI-to-OpenAI. No extra runtime.
verify-translate:
    @./deploy/test/translate-verify.sh

# Used by local-verify.sh. Kept here so there is one definition of the dev
# environment rather than a second copy inside a shell script.
_verify-env:
    @echo 'export OAG_DATABASE__URL="{{dev_db}}"'
    @echo 'export OAG_REDIS__URL="{{dev_rd}}"'
    @echo 'export OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)"'
    @echo 'export OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)"'

_verify-bootstrap:
    @OAG_DATABASE__URL="{{dev_db}}" OAG_REDIS__URL="{{dev_rd}}" \
      OAG_SECURITY__SIGNING_SECRET="$(just _dev-secret)" \
      OAG_SECURITY__CREDENTIAL_KEK="$(just _dev-kek)" \
      sh -c 'cargo run --quiet -p oag -- admin init --email verify@localhost --route default && \
             cargo run --quiet -p oag -- admin seed-catalog >/dev/null && \
             cargo run --quiet -p oag -- admin add-account --name mock --provider anthropic \
               --secret FAKE-CREDENTIAL-FOR-TESTS --route default >/dev/null'

# The newest row, pipe-separated, for the assertions in local-verify.sh.
_verify-ledger:
    @psql "{{dev_db}}" -At -F'|' -c "SELECT model_id, tier, input_tokens, output_tokens, \
        cost_usd, counterfactual_usd, coalesce(ttft_ms, 0) \
        FROM usage_event WHERE model_id LIKE 'anthropic%' ORDER BY occurred_at DESC LIMIT 1"

# ── helpers ────────────────────────────────────────────────────────────────────
# Deterministic dev-only secrets. Never use these anywhere real.
_dev-secret:
    @echo "dev-only-signing-secret-do-not-use-in-production-0001"

_dev-kek:
    @echo "b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA="
