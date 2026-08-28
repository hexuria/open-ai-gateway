# Cursor/Grok through the gateway: can OAG broker org-shared agents?

**Status: research. Nothing here is committed work.** Written 2026-08-28 after
reading OAG (`main`), `grok-bot`, and `barok-works` end to end. The question it
answers: if we moved Cursor authentication to the gateway (or to a service the
gateway fronts), could an organisation *share and rotate* Grok Bot agents across
its members? Short answer: yes, and it's better-founded than it first sounds —
but the leverage OAG adds is **credential brokerage**, not speaking Cursor's
protocol. This doc argues for where the seam goes and names the real work.

Claims are cited `repo-relative/path:line`. Paths under `crates/` are OAG;
paths beginning `source/` are `grok-bot`; paths beginning `server/` are
`barok-works`.

---

## 1. The shape of the answer

Three systems, three jobs, and they do not overlap:

| System | Owns | Speaks |
|---|---|---|
| **OAG** | model-provider credentials, cost, usage ledger, seat accounting | HTTP/JSON chat-completions |
| **grok-bot** | the Cursor wire — ConnectRPC to `api2.cursor.sh`, sandbox lifecycle | Connect protocol + per-box JSON gateway |
| **barok-works** | multi-user server; one org Cursor account fanned out to many users | AG-UI + a thin JSON gateway subset |

The temptation is to teach OAG to talk to Cursor. That is the wrong move for the
same reason the capability-gaps doc gives for guardrails and tool-proxying
(`docs/research/gateway-capability-gaps.md` §1, §3.4): **do not absorb a foreign
protocol into the gateway.** OAG is strictly HTTP/JSON chat-completions and has
no protobuf anywhere; forcing ConnectRPC into its request path couples its
release cycle to Cursor's and makes it bad at being a gateway.

The right move is the one the schema already anticipates. The service catalog
has `kind = 'sandbox'` and `kind = 'browser'` — which is *exactly* what a Cursor
box is (a Linux desktop with a noVNC screen). A sandbox provider registers as a
service; OAG contributes the one thing it is uniquely built for — a sealed,
refreshed, rotatable credential pool — by becoming the **token broker** the
sandbox provider reads from.

---

## 2. The make-or-break fact: box credentials are not device-bound

Org sharing lives or dies on one question — *what authorises driving an existing
sandbox?* If a session were pinned to one machine, a gateway could not hand an
agent to another user without impersonating hardware. It is not pinned.

**`EnsureSandBox` returns flat bearer tokens.** The response carries
`gatewayUrl, gatewayToken, networkToken, vncUrl` (plus unused
`execDaemonAuthToken`/`execDaemonUrl`) —
`source/packages/proto/generated/aiserver/v1/sand_box_pb.ts:996-1008`. Every
subsequent agent command is `POST {gatewayUrl}/api/{method}` with
`Authorization: Bearer {gatewayToken}` and `x-anyrun-network-token: {networkToken}`
(`source/node-agent-coordinator/gateway/gateway-client.ts:95-99,261-266`;
wire constants `source/shared/gateway-wire.ts:1-9`).

**The box verifies that token with a plain constant-time compare** — no device
check, no session binding (`source/host/gateway-server.ts:21`). grok-bot even
persists the descriptor to disk and reuses it for up to **7 days**
(`source/electron-main/box/gateway-descriptor-cache.ts:8`). Any client holding
the token drives the box.

**The machine-id checksum guards only the Cursor backend, and isn't hardware-bound.**
`x-cursor-checksum` / `x-cursor-client-version` are attached to the
`api2.cursor.sh` ConnectRPC calls (`EnsureSandBox`, refresh) by one interceptor
(`source/shared/node/cursor-backend/cursor-inference.ts:133-153`); the machine id
is a **random UUID persisted in the OS secret store**
(`source/electron-main/account/cursor-machine-id.ts:4,12-21`), so a broker can
hold or generate one centrally.

**Re-issuing is free.** Calling `EnsureSandBox` again with a valid account JWT
returns a *fresh* `gatewayToken` for the same box each time — barok does exactly
this per request behind a 20-second cache
(`server/src/computer/cursor-box.ts:44-73`, `locateCacheMs = 20_000`).

**The checksum never touches the prompt leg.** Confirmed on both clients: the
JWT, `x-cursor-checksum`, and `x-cursor-client-version` are sent *only* on the
minting call (`EnsureSandBox`, ConnectRPC to `api2.cursor.sh`). The leg that
actually prompts an existing box carries `Authorization: Bearer {gatewayToken}`
and nothing else — grep finds the checksum headers only in the ConnectRPC
interceptor, never in `gateway-client.ts` / `box-host-connector.ts`, and barok
reimplements the identical split (`server/src/grok/gateway.ts:237-252` sets only
content-type + bearer + network-token). So `x-cursor-checksum` binds the act of
*asking Cursor to mint a pointer*, not the act of *using* one.

Consequence: a central holder of the **Cursor account JWT** can mint box
credentials on demand and hand them to any authenticated user. Nothing the
clients send pins the chain in a way a gateway cannot centralise.

**The one thing source cannot settle.** The box gateway is Cursor-hosted
infrastructure, present in *neither* repo. So while the *clients* treat
`gatewayToken` as an opaque bearer with no device binding, we cannot rule out
that the token itself carries a short expiry, is single-use, or embeds a
machine/session claim the server checks and the client never has to echo. Every
test fixture in both repos uses a literal stub (`"gt"`), so the token's internal
structure is unverifiable from source. **This is the single risk that decides
whether cross-user handoff is safe** — resolve it with a live experiment (mint a
box on one host, drive it from another) before building on §5.

---

## 3. barok-works already proves the org-shared model

This is not a greenfield idea. barok-works runs one Cursor account for a whole
org and fans it out:

- **One org credential, literally.** `CURSOR_KEY = { kind: "mcp_user_token",
  provider: "cursor", keyId: "org" }` — `server/src/org/vault.ts:22-26`. The
  vault is AES-256-GCM (32-byte key enforced at `server/src/config.ts:297-302`),
  and a unique index permits **one live credential per key**
  (`server/src/db/schema/core.ts:332-338`).
- **Users authenticate separately** (better-auth / SSO / Okta), entirely
  disjoint from the shared Cursor secret (`server/src/auth/index.ts:1-19`).
- **The sandbox is explicitly shared.** `isolation: "shared"`, *"One shared
  Cursor-account Linux desktop, per-bot noVNC screens"*
  (`server/src/computer/cursor-box.ts:29-31,55`); the roster is imported once —
  *"Cursor login owns the Grok Bot roster"* (`server/src/agents/cursor-cloud.ts:47-51`).

So the "share agents across an org" half is a solved problem in the barok
codebase. What barok does **not** have is credential *rotation* — see §6.

---

## 4. The seam already exists in both clients

Neither client needs surgery to point its auth at a remote broker. Both isolate
Cursor token handling behind a single choke point:

- **barok-works: a 3-method interface.** `CursorTokenStore = { read, write, clear }`
  (`server/src/grok/token-store.ts:12-16`). The production impl,
  `createVaultCursorTokenStore`, wraps the vault
  (`server/src/org/vault.ts:68-106`); an in-memory impl exists for tests
  (`token-store.ts:58-73`). **Every** caller (`syncCursorSession`,
  `connectCursorBox`, route handlers) goes through `cursor.read()` /
  `cursor.write()`. Wired once at boot (`server/src/index.ts:163-181`). This is
  the swap point with zero other code changes.
- **grok-bot: one class + a configurable base URL.** `SandCursorAuthService`
  logs in, stores, reads, refreshes (`source/electron-main/account/cursor-auth.ts:192-351`);
  everyone else reaches the JWT through `getValidAccessToken()`. The backend URL
  is env-overridable — `SAND_BACKEND_URL` / `CURSOR_API_BASE_URL`
  (`source/shared/node/cursor-token.ts:38-40`), so even the ConnectRPC host is
  not hardcoded.

The refresh contract a broker must implement: `POST {backend}/oauth/token`, JSON
body `{client_id, grant_type: "refresh_token", refresh_token}`, prod `client_id`
`KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB`, 5-minute pre-expiry leeway
(`source/electron-main/account/cursor-auth.ts:340`,
`source/shared/node/cursor-token.ts:4-6`).

---

## 5. The proposed design: OAG as `CursorTokenStore`

```
  barok-works user  ──auth──▶  barok-works server
                                     │
                                     │ cursor.read() / write()   (CursorTokenStore)
                                     ▼
                                OAG broker endpoint
                                     │  ├─ KEK-sealed Cursor seat(s)   crates/oag-core/src/seal.rs
                                     │  ├─ token_version refresh CAS   crates/oag-server/src/gateway/refresh.rs
                                     │  ├─ seat rotation across a pool  (new)
                                     │  └─ usage ledger / seat accounting
                                     ▼
                          barok EnsureSandBox ─▶ api2.cursor.sh ─▶ shared box
```

- **OAG registers barok as a `kind = 'sandbox'` service** in the catalog
  (`crates/oag-server/src/admin/services.rs`) — health-checked and deep-linked
  like any other, no request-path protocol change.
- **barok swaps `createVaultCursorTokenStore` → `createOagBrokerCursorTokenStore`** —
  a 3-method HTTP client against OAG. Because the interface is
  `{read, write, clear}` and every barok caller already routes through it, this
  is the only barok change of substance.
- **OAG owns the Cursor credential lifecycle**: sealing (already built),
  `token_version` compare-and-swap refresh (already built — the exact machinery
  used for xAI/Codex seats at `crates/oag-server/src/gateway/refresh.rs:25-80`),
  and — new — **rotation across a pool of Cursor seats**, so "which seat backs
  this request" becomes a gateway decision. Agent rotation falls out of this:
  barok already fans one roster to many users; OAG rotates the seat underneath.

This keeps each system doing what it is good at. OAG never learns ConnectRPC;
barok never learns credential rotation.

---

## 6. Honest blockers — do not gloss these

1. **OAG has no credential-*issuing* path today.** It is exclusively a
   *consumer* of upstream credentials — seals, refreshes, schedules them for its
   own inbound callers, and never hands one out. Serving a live Cursor bearer
   token to a sibling service is a **new trust posture**, not a refactor. The one
   schema field that looks the part, `service.auth_ref`, is validated on write
   and **never dereferenced** anywhere in the codebase
   (`crates/oag-store/src/repo.rs:691-698` validates it; no read site exists).
   This endpoint — authn'd, audited, minting/serving a short-lived token to a
   trusted peer — **is the real work of this project.**

2. **The 0.29 "team" RPCs are the org-sharing surface, and they are the one thing
   not in code.** grok-bot's *generated* proto tops out at **0.27** (0.18
   recovered base + a 0.27 additive layer — `grok_bot_connect.ts` 30 methods,
   `grok_bot_connect.ported.ts` 76 methods). **0.29 exists only as a
   strings/RPC-name diff** (`source/../docs/gap-analysis-0.29.md`: "332 methods;
   81 absent from our source"), plus the DMG on disk — no extracted `.proto` /
   `_pb.ts`. The on-topic 0.29 delta is *"Multi-account/team switching + spend
   limits (8 RPCs)"*, filed **out of scope by design** and unported. And the team
   RPCs that *are* recovered (`ListTeamMemberSandBoxes`, `KillTeamMemberSandBox`
   — `grok_bot_connect.ts:266-282`) are **admin list/kill only**; none hands a
   *running* session to another user. So org sharing goes the **barok way (one
   shared account, app-level fan-out)**, not through Cursor's team protocol.
   Note too that even the team/room/template RPCs that *are* recovered have
   **zero call sites in either client** — they are declared message types, not an
   exercised path (grok-bot docs say outright *"Room-turn RPCs stay unused"*,
   `docs/0.27-PORT.md:40`). Building on them means building against
   declared-but-untested RPCs, with no working reference implementation to copy.

3. **Rotation across multiple Cursor accounts exists in neither repo.** barok's
   schema (`kind, provider, keyId`) *allows* multiple `keyId`s but no code reads
   a second one (`server/src/db/schema/core.ts:332-338`); grok-bot is
   single-account by construction (two fixed secret keys,
   `source/electron-main/account/cursor-auth.ts:19-20`). A seat *pool* is
   net-new — mostly on OAG's side, which is the correct home for it.

4. **Proxy support is partial.** grok-bot honours `HTTPS_PROXY` only on the
   login/token-exchange path (`source/packages/cursor-config/auth/proxy-fetch.ts`,
   used at `.../auth/login.ts:4,44,89`); the ConnectRPC transport and per-box
   `fetch` use Node's default dispatcher — not confirmed to honour a proxy. barok
   has no proxy handling at all. Matters for an egress-restricted org deployment.

5. **Rights / ToS is a real decision, not a footnote.** Both repos carry
   cautions (grok-bot `PROVENANCE.md`, barok `docs/grok-0.27-disparity.md`). The
   flow uses Anysphere's **production** `client_id` against Cursor's production
   backend, stamped `x-cursor-client-type: sand`. Desktop-local single-user is a
   different posture from a hosted, multi-user org broker. This needs an explicit
   decision before rollout — likely from whoever owns the Cursor contract, not
   from engineering.

---

## 7. What OAG would need to build (scoped)

- **A broker endpoint** on the internal admin listener: `read` (serve the current
  sealed Cursor access token, refreshing if within leeway), `write` (accept a
  rotated token after an interactive Cursor login done elsewhere), `clear`.
  Authn'd like every admin route (`crates/oag-server/src/admin/auth.rs`), and —
  because it hands out a live credential — **audited to a persisted table, not a
  log line** (the audit-trail gap from `gateway-capability-gaps.md` §3.4 bites
  harder here than anywhere else in the system).
- **A `cursor` credential kind + seat pool.** Reuse the OAuth seat machinery
  (`CredentialKind::OAuth`, `owner_principal_id`, the reserve/rotation columns);
  add rotation selection so a request draws the least-loaded live Cursor seat.
- **Seat accounting** as with Grok/Codex seats — flat-rate, `cost_usd = 0`,
  counterfactual booked — so the ledger shows what the org would have paid.

Everything above the broker line (sandbox provisioning, the proto wire, the
roster) stays in barok. OAG stays a gateway.

---

## 8. Open questions for the operator

- Is a live Cursor **bearer token leaving OAG to a sibling service** acceptable,
  or must the broker instead *proxy* the `EnsureSandBox` call so the raw JWT never
  leaves? (Proxying keeps the secret in OAG but means OAG makes one ConnectRPC-ish
  call — a narrower protocol exception than hosting the whole wire.)
- One org Cursor account, or a **pool of seats** to rotate? The pool is where
  OAG's value concentrates, but it is also the net-new build.
- Has the ToS posture in §6.5 been cleared for hosted multi-user use?
- Does the deployment need proxy-honouring egress (§6.4)? If so, that work lands
  in the clients, not OAG.
- Is barok-works the sandbox provider we standardise on, or a barok-shaped
  service we write? The `CursorTokenStore` seam is identical either way.

## See also

`docs/research/gateway-capability-gaps.md` · `docs/research/grok-two-ways.md` ·
grok-bot `docs/grok-0.27-disparity-proto.md` · barok-works
`server/src/grok/token-store.ts`
