-- Baseline schema.
--
-- One migration, not 268. sub2api's history is worth reading for what it
-- learned and not worth replaying: it contains same-numbered files, several
-- rounds of patch migrations, and four subsystems whose tables were managed
-- outside the ORM entirely. We start from the shape those 268 arrived at.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ── principals ────────────────────────────────────────────────────────────────
-- An organisation member. Deliberately not a user-management system: identity
-- comes from the org's own IdP, and this table holds only what the gateway
-- needs to attribute and cap spend.
CREATE TABLE principal (
    id                  uuid PRIMARY KEY,
    email               text        NOT NULL UNIQUE,
    -- Subject claim from the org IdP. NULL for the break-glass local admin.
    oidc_subject        text UNIQUE,
    role                text        NOT NULL DEFAULT 'member'
                        CHECK (role IN ('member', 'admin')),
    monthly_budget_usd  numeric(14,6),
    -- Multiple of the budget at which to stop serving entirely. Between the
    -- budget and this, requests are downgraded to the cheapest rung rather than
    -- refused: a developer who can get no answer at all routes around the
    -- gateway, and then you have neither the savings nor the visibility.
    hard_stop_multiple  numeric(6,3) NOT NULL DEFAULT 1.2,
    active              boolean     NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- ── routes ────────────────────────────────────────────────────────────────────
-- A tier ladder plus its entitlements and budget. This is the single routing
-- concept: sub2api had five overlapping ones (Group, Channel,
-- channel_model_pricing, CompositeModelRoute, and Group.model_routing) with a
-- name collision between two of them.
CREATE TABLE route (
    id                  uuid PRIMARY KEY,
    name                text        NOT NULL UNIQUE,
    -- Ordered ladder, cheapest first:
    --   [{"name":"cheap","models":["kimi/k2"]}, {"name":"frontier","models":[...]}]
    -- Ordering is positional, which is what makes escalation and budget
    -- downgrade index arithmetic instead of stringly-typed comparisons.
    tiers               jsonb       NOT NULL,
    -- What to do when a client names a *concrete* model.
    --   passthrough: honour it. The default, because silently redirecting a
    --                request someone deliberately routed is the kind of
    --                surprise that gets a gateway removed from the path.
    --   managed:     override it with policy anyway. For an operator who wants
    --                cost rules enforced over clients that hardcode a model.
    -- Virtual `oag/*` names are always managed, whichever this says.
    default_mode        text        NOT NULL DEFAULT 'passthrough'
                        CHECK (default_mode IN ('managed', 'passthrough')),
    -- Minimum rung this route will ever serve from. An entitlement, not a
    -- preference: budget pressure does not override it.
    floor_tier          text,
    price_overrides     jsonb       NOT NULL DEFAULT '{}'::jsonb,
    rpm_limit           integer,
    monthly_budget_usd  numeric(14,6),
    active              boolean     NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- ── accounts ──────────────────────────────────────────────────────────────────
-- One upstream credential.
CREATE TABLE account (
    id                  uuid PRIMARY KEY,
    name                text        NOT NULL,
    provider            text        NOT NULL,
    kind                text        NOT NULL
                        CHECK (kind IN ('api_key','oauth','bedrock','vertex','service_account')),

    -- AEAD-sealed. Never plaintext, never logged, never returned by the admin
    -- API. sub2api stores OAuth access and refresh tokens as plaintext JSONB,
    -- which makes a database backup a credential dump.
    credentials_sealed  bytea       NOT NULL,
    credentials_nonce   bytea       NOT NULL,
    -- Monotonic, guards against a concurrent refresh clobbering a newer token.
    token_version       bigint      NOT NULL DEFAULT 0,
    -- Denormalised so the scheduler can skip expired credentials without
    -- decrypting every candidate on every request.
    token_expires_at    timestamptz,

    -- NULL means this credential joins the shared pool. Set means it belongs to
    -- one person and only their requests may use it.
    --
    -- This one nullable column is the whole difference between "the org pools
    -- its API keys" and "each member uses their own seat" — see
    -- docs/compliance.md. The pool and the router are indifferent to which.
    owner_principal_id  uuid REFERENCES principal(id) ON DELETE CASCADE,

    proxy_url           text,
    priority            smallint    NOT NULL DEFAULT 0,
    max_concurrency     integer     NOT NULL DEFAULT 8 CHECK (max_concurrency >= 0),
    -- No `weight`. The scheduler is a cascade of independently justified
    -- filters rather than a weighted score, precisely so "why did this request
    -- go there" has an answer at 3am. A weight column with nothing reading it
    -- is an invitation to reintroduce the scoring it exists to avoid.

    -- Scheduling state. All expiry-based: there is no sweeper and no state
    -- machine, so nothing can leave a credential stuck out of the pool.
    schedulable         boolean     NOT NULL DEFAULT true,
    cooldown_until      timestamptz,
    cooldown_reason     text,
    rate_limited_until  timestamptz,
    window_resets_at    timestamptz,
    last_used_at        timestamptz NOT NULL DEFAULT to_timestamp(0),

    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- The scheduler's candidate query: by provider, eligible only.
CREATE INDEX account_schedulable_idx
    ON account (provider, priority, last_used_at)
    WHERE schedulable;
CREATE INDEX account_owner_idx ON account (owner_principal_id)
    WHERE owner_principal_id IS NOT NULL;

-- Which credentials a route may draw on. Membership only: preference is
-- `account.priority`, applied by the scheduler's cascade. This table carried a
-- `priority` column that no query ever ordered by, which read as a promise the
-- code did not keep.
CREATE TABLE account_route (
    account_id  uuid     NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    route_id    uuid     NOT NULL REFERENCES route(id)   ON DELETE CASCADE,
    PRIMARY KEY (account_id, route_id)
);
CREATE INDEX account_route_by_route_idx ON account_route (route_id);

-- ── inbound keys ──────────────────────────────────────────────────────────────
CREATE TABLE api_key (
    id              uuid PRIMARY KEY,
    -- sha256 of the key, hex. The plaintext is shown once at creation and is
    -- not recoverable.
    --
    -- sub2api stores inbound keys in plaintext and looks them up by column
    -- equality, so read access to one table is read access to every client's
    -- credential. Hashing costs one sha256 per cache miss.
    key_hash        text        NOT NULL UNIQUE,
    -- First few characters, for the operator to recognise a key in a list.
    key_prefix      text        NOT NULL,
    name            text        NOT NULL,
    principal_id    uuid        NOT NULL REFERENCES principal(id) ON DELETE CASCADE,
    route_id        uuid        NOT NULL REFERENCES route(id),
    -- Pin this key to a minimum rung: the CI agent gets `cheap`, the
    -- architecture-review key gets `frontier`.
    floor_tier      text,
    -- Admin authority is a property of the KEY, not of the principal that owns
    -- it. An operator's ordinary inference key is pasted into SDK configs and
    -- CI; it must not also be able to disable credentials or revoke keys.
    -- `oag admin init` mints exactly one key with this set.
    admin           boolean     NOT NULL DEFAULT false,
    quota_usd       numeric(14,6),
    spent_usd       numeric(14,6) NOT NULL DEFAULT 0,
    expires_at      timestamptz,
    active          boolean     NOT NULL DEFAULT true,
    last_used_at    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX api_key_principal_idx ON api_key (principal_id);

-- ── model catalog ─────────────────────────────────────────────────────────────
-- Seeded from LiteLLM's pricing table, overridable per deployment.
CREATE TABLE model_catalog (
    id                      text PRIMARY KEY,        -- "anthropic/claude-opus-5"
    provider                text          NOT NULL,
    upstream_name           text          NOT NULL,  -- what goes on the wire
    input_per_mtok          numeric(12,6) NOT NULL,
    output_per_mtok         numeric(12,6) NOT NULL,
    cache_read_per_mtok     numeric(12,6),
    cache_write_per_mtok    numeric(12,6),
    context_window          integer       NOT NULL,
    max_output_tokens       integer       NOT NULL,
    supports_vision         boolean       NOT NULL DEFAULT false,
    supports_tools          boolean       NOT NULL DEFAULT false,
    supports_reasoning      boolean       NOT NULL DEFAULT false,
    supports_prompt_cache   boolean       NOT NULL DEFAULT false,
    -- true when an operator edited it, so a catalog refresh does not stomp it.
    is_override             boolean       NOT NULL DEFAULT false,
    updated_at              timestamptz   NOT NULL DEFAULT now()
);

-- ── usage ledger ──────────────────────────────────────────────────────────────
-- Append-only, and the hottest table in the system.
CREATE TABLE usage_event (
    -- The inbound request id. Primary key, which makes metering idempotent for
    -- free: a retried write conflicts instead of double-billing.
    request_id              uuid PRIMARY KEY,
    occurred_at             timestamptz   NOT NULL DEFAULT now(),

    principal_id            uuid REFERENCES principal(id) ON DELETE SET NULL,
    api_key_id              uuid REFERENCES api_key(id)   ON DELETE SET NULL,
    route_id                uuid REFERENCES route(id)     ON DELETE SET NULL,
    account_id              uuid REFERENCES account(id)   ON DELETE SET NULL,

    model_id                text          NOT NULL,
    tier                    text          NOT NULL,
    selection_reason        text          NOT NULL,
    -- Set when a quality gate tripped and we retried a rung up. Lets you price
    -- the escalation policy: escalations that never improve anything are pure
    -- cost.
    escalated_from_tier     text,
    escalation_gate         text,

    input_tokens            bigint        NOT NULL DEFAULT 0,
    output_tokens           bigint        NOT NULL DEFAULT 0,
    cache_read_tokens       bigint        NOT NULL DEFAULT 0,
    cache_write_tokens      bigint        NOT NULL DEFAULT 0,

    cost_usd                numeric(14,8) NOT NULL DEFAULT 0,
    -- What this exact usage would have cost on the route's top rung.
    --
    -- The whole justification for the gateway is `SUM(counterfactual - cost)`.
    -- Recorded per request from real token counts, because estimating it later
    -- from averages would make the headline number unfalsifiable.
    counterfactual_usd      numeric(14,8) NOT NULL DEFAULT 0,
    counterfactual_model_id text,

    status                  smallint      NOT NULL,
    latency_ms              integer,
    ttft_ms                 integer,
    streamed                boolean       NOT NULL DEFAULT false
);

-- Dashboard: spend over time, sliced by who and by what.
CREATE INDEX usage_event_time_idx      ON usage_event (occurred_at DESC);
CREATE INDEX usage_event_principal_idx ON usage_event (principal_id, occurred_at DESC);
CREATE INDEX usage_event_route_idx     ON usage_event (route_id, occurred_at DESC);
CREATE INDEX usage_event_account_idx   ON usage_event (account_id, occurred_at DESC);

-- No `setting` table. Configuration lives in the config file, where a change is
-- reviewable in a pull request; anything that must change without a restart
-- gets a typed column on the entity it belongs to (`route.default_mode`,
-- `account.schedulable`), which is no more expensive than a generic key/value
-- row and cannot drift into an untyped grab bag.
