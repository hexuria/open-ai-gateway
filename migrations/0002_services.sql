-- Capability service catalog.
--
-- OAG is the organisation's model door. It does not grow a sandbox, a
-- browser, or a guardrail by absorbing one. This table is the registration
-- surface: a row, a health check, a deep-link to the service's own dashboard.
-- Panday, Berthos, Headroom, and Orgo are backends an operator points at,
-- not code this repository copies.
--
-- Expand-only. The previous release does not read this table, so adding it
-- while that release is still serving is safe.

CREATE TABLE service (
    id              uuid PRIMARY KEY,
    name            text        NOT NULL UNIQUE,
    kind            text        NOT NULL
                    CHECK (kind IN (
                        'sandbox', 'tool', 'guard', 'reduce',
                        'harness', 'browser', 'other'
                    )),
    -- Application code refuses anything that is not http(s) and cheaply
    -- denies link-local / metadata targets. The CHECKs are the second line
    -- for anything that reaches the database without going through that path.
    base_url        text        NOT NULL
                    CHECK (base_url LIKE 'http://%' OR base_url LIKE 'https://%'),
    health_path     text        NOT NULL
                    CHECK (health_path LIKE '/%' AND health_path NOT LIKE '//%'),
    dashboard_url   text
                    CHECK (dashboard_url IS NULL
                           OR dashboard_url LIKE 'http://%'
                           OR dashboard_url LIKE 'https://%'),
    -- Pointer into the existing credential pool. Not a second vault.
    auth_ref        uuid        REFERENCES account(id) ON DELETE SET NULL,
    enabled         boolean     NOT NULL DEFAULT true,
    last_ok         timestamptz,
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX service_enabled_idx ON service (enabled, name);
