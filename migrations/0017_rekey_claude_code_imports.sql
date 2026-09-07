-- C11 re-keyed Claude Code imports, and left every already-imported row behind.
--
-- `usage_import` derives an imported row's identity from its `source_ref`:
-- the unique index `usage_event_source_ref_key` (0008) is on that column, and
-- `request_id` is `uuid_v5(IMPORT_NAMESPACE, source_ref)`. Both are how a
-- re-import recognises what it already wrote — 0008's own comment says so:
-- "a re-import derives the same `source_ref` and loses the race against this
-- index instead of appending a second copy of the same money".
--
-- C11 changed the Claude Code shape from `claude-code:{session}:{message}` to
-- `claude-code:{message}`, because Anthropic's message ids are global and one
-- message quoted in two transcript files was being booked twice. Correct, and
-- it does not apply to Grok CLI, whose `prompt_id` is not known to be unique
-- outside its session.
--
-- What it did not do is carry the existing rows across. On a database that
-- imported on an older binary, the next `oag admin usage import --apply`
-- derives a source_ref and a request_id that match nothing, so
-- `ON CONFLICT DO NOTHING` conflicts with nothing and inserts a second copy of
-- every message ever imported. Worse, it looks clean: the old binary printed
-- "N were already imported and were left alone", and the new one reports every
-- row as written, which is what a genuine first import looks like.
--
-- Nothing else catches it. `judge`'s fingerprint pre-flight reads
-- `repo::gateway_fingerprints`, which is `WHERE origin = 'gateway'` and
-- deliberately excludes imported rows; `--before` excludes sessions that ended
-- after a cutoff, which is the opposite set; `--account` is not part of the
-- key. `revert` would remove both copies, but nothing tells an operator to run
-- it first.
--
-- So the rows are re-keyed here, once, where it can be done to a whole
-- database rather than one operator at a time.
--
-- Tested by `usage_import::tests::the_backfill_rekeys_old_imports_onto_what_the
-- _importer_now_derives`, which seeds rows in the old shape and runs THIS FILE
-- against them with `include_str!` — a migration runs before any test can plant
-- a row for it, and a copy of these statements in a test would agree with them
-- only until somebody edited one.

-- UUIDv5 is SHA-1 over the namespace's 16 bytes followed by the name, with the
-- version and variant nibbles overwritten. `uuid-ossp` is not installed and is
-- not worth requiring for one statement; pgcrypto is, and already ships
-- `digest`. Verified against the `uuid` crate's output for this namespace
-- before this migration was written.
--
-- Local to this migration and dropped at the end: nothing else needs it, and a
-- lingering helper is a thing the next person has to work out the purpose of.
CREATE FUNCTION oag_import_uuid_v5(ns uuid, name text) RETURNS uuid AS $$
DECLARE h bytea;
BEGIN
    h := substring(
        digest(decode(replace(ns::text, '-', ''), 'hex') || convert_to(name, 'UTF8'), 'sha1')
        FROM 1 FOR 16
    );
    h := set_byte(h, 6, (get_byte(h, 6) & 15) | 80);   -- version 5
    h := set_byte(h, 8, (get_byte(h, 8) & 63) | 128);  -- RFC 4122 variant
    RETURN encode(h, 'hex')::uuid;
END $$ LANGUAGE plpgsql IMMUTABLE;

-- The duplicates C11 exists to remove, removed here rather than left to
-- collide.
--
-- Two old rows for one message in two sessions collapse onto one new key —
-- which is the double-booking C11 found, so deleting the loser is the fix and
-- not collateral. A row already written in the new shape wins over any old one,
-- because a database in that state has already been imported twice and the new
-- row is the one the next import will match. Otherwise the earliest occurrence
-- wins, tie-broken by `request_id` so the choice is the same on every replica
-- and on a re-run.
--
-- The pattern needs the second colon, so a row already in the new shape is not
-- rewritten by the UPDATE below and is not mistaken for an old one here.
WITH keyed AS (
    SELECT request_id,
           attempt,
           regexp_replace(source_ref, '^claude-code:[^:]+:', 'claude-code:') AS target,
           source_ref !~ '^claude-code:[^:]+:' AS already_new,
           occurred_at
    FROM usage_event
    WHERE origin = 'claude-code' AND source_ref IS NOT NULL
),
ranked AS (
    SELECT request_id,
           attempt,
           row_number() OVER (
               PARTITION BY target
               ORDER BY already_new DESC, occurred_at, request_id
           ) AS rn
    FROM keyed
)
DELETE FROM usage_event e
USING ranked r
WHERE e.request_id = r.request_id
  AND e.attempt = r.attempt
  AND r.rn > 1;

-- And the survivors onto the shape the importer now derives. Both columns
-- together: the index is on `source_ref` and the primary key is on
-- `request_id`, and a re-import has to miss neither.
UPDATE usage_event
SET source_ref = regexp_replace(source_ref, '^claude-code:[^:]+:', 'claude-code:'),
    request_id = oag_import_uuid_v5(
        '6f616725-7573-6167-655f-696d706f7274'::uuid,
        regexp_replace(source_ref, '^claude-code:[^:]+:', 'claude-code:')
    )
WHERE origin = 'claude-code'
  AND source_ref ~ '^claude-code:[^:]+:';

DROP FUNCTION oag_import_uuid_v5(uuid, text);
