-- Where a System One endpoint answers, beneath its base URL.
--
-- Jev answers System One at `{base}/v1/systemone`, and every host that copied
-- it did not copy the path: Merge Gateway serves the same request and answer
-- at `POST https://api-gateway.merge.dev/v1/decisions`. So a `system_one`
-- endpoint may name its own path, and one that names none is posted at
-- `/v1/systemone`, as the built-in Jev is. Its model listing stays at
-- `{base}/v1/models` either way.
--
-- A column rather than a base URL that ends in the path, because the base URL
-- is also where the listing is read from, and one URL cannot end in two paths.
--
-- Only a `system_one` endpoint has one. The chat dialects each build their
-- paths from what the request says (a model, a method, whether it streams),
-- so a fixed path there would be a second way to spell a URL the adapter
-- already builds, and a row that set one would be a row whose author expected
-- something no adapter does.
--
-- A path is appended to the base URL as it is written, so it holds what a URL
-- path may hold and nothing that could end the path or begin a query, a
-- fragment or another host: a leading `/`, then at most 127 of letters,
-- digits, `/`, `.`, `_` and `-`. The gateway also refuses an empty segment and
-- a `.` or `..` one, which this pattern does not.
--
-- Expand-only. The release before this one names its columns in every SELECT
-- and INSERT, so it neither reads this column nor writes it: its rows get
-- NULL, which means the default path, which is the only path it could serve.

ALTER TABLE endpoint
    ADD COLUMN path text
    CONSTRAINT endpoint_path_check
    CHECK (path ~ '^/[A-Za-z0-9/._-]{0,127}$');

ALTER TABLE endpoint
    ADD CONSTRAINT endpoint_path_dialect_check
    CHECK (path IS NULL OR dialect = 'system_one');

COMMENT ON COLUMN endpoint.path IS
    'Where a system_one endpoint takes a question set, beneath base_url. NULL '
    'is /v1/systemone. Only system_one rows may set it.';
