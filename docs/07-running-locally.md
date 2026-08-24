# Running locally, and the two keys

Three ways to run the gateway on your own machine, and how to get the keys that
reach it.

## The three run modes

| Command | What runs | Use it for |
|---|---|---|
| `just dev-serve` | Postgres + Redis (Docker), migrated, then the gateway as a **host binary** (one replica) | Everyday development. Fast rebuilds — the gateway is not containerised. |
| `just serve` | Just the gateway host binary (assumes `just dev` already brought the infra up) | Re-running the gateway without touching the infra. |
| `just stack-up` | The **full topology** in Docker: Caddy → Envoy → three replicas → Postgres → Redis | Rehearsing the production shape locally. Builds a release image, so it takes a few minutes. |

The gateway binds **:29080** (inference) and **:29081** (admin API + dashboard)
locally — not 8080/8081, which collide with everything. `just serve` walks up to
the first free pair and prints what it chose.

**What `just dev-serve` / `just serve` is NOT:**

- **Not floci.** floci (`deploy/tofu/verify-floci-gcp.sh`) is a *GCP API emulator*
  used to rehearse the cloud deploy config with no cloud account. It emulates
  Cloud Run's control plane; it does **not** run the gateway container, so there
  is no process to send a request to. It answers "would the GCP deploy apply?",
  not "is OAG running?".
- **Not `just stack-up`.** That is the separate full-topology stack above.

## The two keys — and why they differ

Every key is minted against a principal and a route. Authority to reach the
**admin** API is a property of the **key**, not the principal (`api_key.admin`):
an ordinary inference key is deliberately refused on `/admin/api`, because that
key gets pasted into SDK configs and CI, and leaking it must not also hand over
the admin surface. So there are two kinds:

| | Inference key | Admin key |
|---|---|---|
| Mint | `just key` | `just admin-key` |
| Sends requests (`/v1/messages` on :29080) | ✅ | ✅ |
| Reaches the dashboard + `/admin/api` (:29081) | ❌ 403 | ✅ |
| Goes in | an SDK / client config | the dashboard key box only |

```bash
just dev-serve          # infra + migrate + run the gateway (leave this running)

# in another shell:
just key                # → an inference key (oag_live_…) for /v1/messages
just admin-key          # → an admin key (oag_live_…) for the dashboard
```

Name them if you like: `just key name=codex`, `just admin-key name=ops`.

The very first admin key is also printed by `just bootstrap` (which runs
`oag admin init`) — `just admin-key` is just the way to mint more later.

## Using them

```bash
# Inference — point any Anthropic/OpenAI client at the gateway:
curl -N localhost:29080/v1/messages -H "x-api-key: $INFERENCE_KEY" \
  -d '{"model":"oag/auto","max_tokens":256,"stream":true,
       "messages":[{"role":"user","content":"hello"}]}'
```

Dashboard: open <http://127.0.0.1:29081/>, paste the **admin** key into the key
box (top right), and click **Load**. It belongs only in that box — not the
browser address bar.
