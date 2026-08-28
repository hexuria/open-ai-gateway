# Grok, two ways: xAI direct vs Cursor's Grok Bot

Reference note, 2026-08-27. Claims cited by `file:line`.

## Different subscriptions, different vendors

**SuperGrok** is billed by **xAI**. **Grok Bot** ships with a **Cursor** plan,
billed by **Anysphere**. A Cursor login does not authenticate to xAI; an xAI
token does not open a Grok Bot session.

`grok-bot/PROVENANCE.md:3-12` — the repo reconstructs `com.anysphere.sand`, DMG
from `downloads.cursor.com`. Its tokens are Cursor JWTs. `grok-4.5` is a model id
Cursor resolves server-side, not a credential.

## The two paths

| | **A — OAG → xAI** | **B — barok-works → Cursor** |
|---|---|---|
| Billed by | xAI | Anysphere |
| Auth host | `auth.x.ai` | `api2.cursor.sh` |
| Credential | xAI OAuth pair, or console API key | Cursor account JWT |
| Endpoint | `api.x.ai/v1/chat/completions` | `aiserver.v1.GrokBotService/*` (ConnectRPC) |
| Returns | a completion | a sandbox, prompted separately |
| Acquire | import an existing CLI session | browser login + poll |
| Refresh | OIDC discovery → token endpoint | `POST /oauth/token`, **JSON body** |
| In OAG | production | not supported |

## The two credential files

| | `~/.grok/auth.json` (xAI Grok CLI) | `sand-secrets.json` (Grok Bot) |
|---|---|---|
| Location | `~/.grok/auth.json` | `userData/sand-secrets.json` |
| At rest | **plaintext JSON**, `0600` | `safeStorage` ciphertext (Keychain/DPAPI) |
| Multiple accounts | **yes**, one entry per session | **no**, two fixed keys |
| Keyed by | `https://auth.x.ai::<oidc_client_id>` | `cursor-access-token` / `cursor-refresh-token` |
| Access token field | **`key`** (not `access_token`) | `cursor-access-token` |
| Expiry | `expires_at`, RFC3339 | JWT `exp`, read at use |
| Read by OAG | yes, `account add --from grok` | no |

**`~/.grok/auth.json`** (`xai_oauth.rs:47-92`) — flat object; the CLI stores other
providers in the same file, so only `https://auth.x.ai::` keys are xAI's. Entry
fields: `auth_mode, coding_data_retention_opt_out, create_time, email,
expires_at, first_name, key, oidc_client_id, oidc_issuer, principal_id,
principal_type, profile_image_asset_id, refresh_token, team_id, user_id`. Empty
`key` = logged-out remnant, skipped. Several subscriptions coexist, so
`--from grok` makes one seat per entry; `union_sessions` dedupes by token so one
token cannot become two rows with doubled concurrency (`:101`). OAG reads, never
writes — the CLI stays signed in.

**`sand-secrets.json`** (`secret-store.ts:10-17, 37-41, 64-72`) — `string→string`
map, atomic tmp+rename. `scoped:v1:<sha256(jwt.sub)>:` prefixes the access token
as a single-slot guard against a foreign account, not multi-account keying.
Memory-only without an OS keyring; `SAND_PERSIST_SECRETS_ON_DISK=1` writes
`plaintext:v1:<base64>` — encoding, not encryption (`cursor-session-policy.ts:3-7`).
barok-works ignores this file and uses the Postgres vault, AES-256-GCM under
`KEY_ENCRYPTION_KEY`, `keyId` hardcoded `"org"` (`server/src/org/vault.ts:22-26`).

## Path B login flow

1. `verifier` = base64url(32 random bytes); `challenge` = base64url(sha256(verifier)); a `uuid`.
2. Browser opens `cursor.com/loginDeepControl?challenge&uuid&mode=login&redirectTarget`; human signs in.
3. Poll `GET api2.cursor.sh/auth/poll?uuid&verifier` → `{accessToken, refreshToken}`.

`login.ts:17-22, 35-63`. No `code`, no redirect capture, no `state`, no scopes.
The `verifier` stays with the poller, so browser and server need not be the same
machine (`redirectTarget=cli`). Non-interactive alternative, implemented but
unwired: `POST /auth/exchange_user_api_key` with a Cursor API key (`login.ts:84-103`).

Prod `client_id` `KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB` (`cursor-token.ts:4`).
`EnsureSandBox` returns `{gatewayUrl, gatewayToken, networkToken, vncUrl}`;
prompts go to that box with a different bearer plus `x-anyrun-network-token`.
Requires `x-cursor-client-version ≥ 0.24.0` (`client-stamp.ts:1-7`) and
`x-cursor-checksum` from a stable machine id.

## Why OAG does not host Path B

OAG's adapters translate chat dialects into a completion. Cursor provisions a
sandbox and prompts it — no chat request to translate, plus a version gate, a
machine-id checksum, and second and third credential tiers the vault cannot
represent.

## Known gaps in barok-works

1. **No refresh concurrency control** — two 401s both POST the same refresh
   token; on rotation the loser holds a dead one. grok-bot uses promise dedupe,
   a mutation queue, an epoch counter and a rotation rescue
   (`cursor-auth.ts:336, 216-220, 215, 344`).
2. **Reactive refresh only** — 401 handler, one call site
   (`ensure-sandbox-rpc.ts:50`). grok-bot refreshes at `exp − 5 min`.
3. **One account per deployment** — `keyId: "org"`; the vault schema supports
   keying, the code does not use it.
4. **No proxy support** — grok-bot uses `undici ProxyAgent` via `HTTPS_PROXY`.

## Rights

The flow uses Anysphere's production `client_id` against Cursor's production
backend, stamping `x-cursor-client-type: sand`. Cautions exist at
`grok-bot/PROVENANCE.md:31` and `barok-works/docs/grok-0.27-disparity.md:200`.
Desktop-local and multi-user hosted use are different postures.

## See also

`docs/03-providers.md` · `docs/compliance.md` ·
`docs/research/gateway-capability-gaps.md`
