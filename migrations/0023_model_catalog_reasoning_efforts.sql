-- Which reasoning-effort levels a model takes, and which it uses when a
-- request names none.
--
-- `/v1/models` publishes them in opencodex's shape (`supports_reasoning_effort`,
-- `reasoning_effort`, `reasoning_efforts`, `capabilities.reasoning_effort`), so
-- a client's effort slider shows only the stops a model has. They are not
-- fetched per request: `oag admin endpoint sync` and `oag admin catalog
-- sync-efforts` read OpenRouter's public model list and the override table
-- committed in `crates/oag-router/reasoning-efforts.json`, which wins, and
-- store the result here. The gateway serves what the row holds.
--
-- `reasoning_efforts` holds the levels lowest first (`{low,medium,high}`), and
-- `reasoning_effort` the default, one of them. Both NULL means the levels are
-- not known, and `/v1/models` then publishes none of the four fields: a model
-- whose levels nobody stated is not given a guessed ladder. The two are set
-- together or not at all, and the CHECK says so, so no reader has to decide
-- what half a pair means.
--
-- `text[]` rather than `jsonb`: the value is an ordered list of words, every
-- element a string by type, and a row can never hold a list the gateway's
-- reader cannot decode.
--
-- Expand-only. The previous release names its columns in every statement, so
-- it neither reads these nor writes them: a row it inserts gets NULL, which is
-- "not known", and a row it rewrites keeps the levels it had.

ALTER TABLE model_catalog
    ADD COLUMN reasoning_efforts text[],
    ADD COLUMN reasoning_effort text;

ALTER TABLE model_catalog
    ADD CONSTRAINT model_catalog_reasoning_effort_check CHECK (
        CASE
            WHEN reasoning_efforts IS NULL THEN reasoning_effort IS NULL
            ELSE reasoning_effort IS NOT NULL
                 AND array_position(reasoning_efforts, NULL) IS NULL
                 AND reasoning_effort = ANY (reasoning_efforts)
        END
    );

COMMENT ON COLUMN model_catalog.reasoning_efforts IS
    'The reasoning-effort levels the model takes, lowest first; NULL when not '
    'known. Written by endpoint sync and catalog sync-efforts.';
COMMENT ON COLUMN model_catalog.reasoning_effort IS
    'The level the model uses when a request names none: one of '
    'reasoning_efforts, and NULL exactly when that is.';
