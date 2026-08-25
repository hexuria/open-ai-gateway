-- Where a ledger row came from, and whether its cost is a measurement.
--
-- Until now every row in `usage_event` was served by this gateway, so provenance
-- was implicit. Importing a CLI's own session transcripts breaks that: the same
-- table now holds traffic the gateway never saw, which must be separable from
-- traffic it did — to report either side, to report the difference, and to
-- delete a bad import without touching anything the gateway earned.
--
-- Expand-only. Every column is either defaulted or nullable, and every existing
-- SELECT in the tree names its columns explicitly, so the previous release runs
-- unchanged against this schema and a rollback loses the provenance and nothing
-- else.

ALTER TABLE usage_event
    -- 'gateway' for a row this gateway served, otherwise the importer that
    -- wrote it ('claude-code'). Defaulted rather than nullable because every
    -- row that already exists genuinely was served here, and a NULL would make
    -- the honest query `origin IS NULL OR origin = 'gateway'` forever.
    ADD COLUMN origin text NOT NULL DEFAULT 'gateway',

    -- The external record this row stands for, as the source names it:
    -- 'claude-code:<sessionId>:<message.id>'. NULL on a gateway row, which has
    -- no external identity.
    ADD COLUMN source_ref text,

    -- False when nothing could price this row.
    --
    -- `cost_usd` is NOT NULL and every reporting query sums it, so an unpriced
    -- import has to store 0 there. Zero and free are indistinguishable in that
    -- column, and the difference matters: a model missing from the catalog is
    -- an unanswered question, not a gift. This is the flag that keeps the two
    -- apart without making `cost_usd` nullable -- which would break the row
    -- decoder in the usage listing, and the arithmetic in the api_key debit.
    ADD COLUMN cost_known boolean NOT NULL DEFAULT true;

-- The idempotency key, enforced here rather than only in the importer.
--
-- A re-import derives the same `source_ref` and loses the race against this
-- index instead of appending a second copy of the same money. Partial, because
-- a gateway row has no external identity and millions of NULLs would otherwise
-- have to be unique against each other.
--
-- The importer also derives each row's `request_id` deterministically from this
-- same string, so the primary key rejects a duplicate too. Two independent
-- enforcements of one fact: whichever is dropped by a later schema change, the
-- ledger still cannot hold the same imported message twice.
CREATE UNIQUE INDEX usage_event_source_ref_key
    ON usage_event (source_ref) WHERE source_ref IS NOT NULL;

-- Slicing the ledger by provenance over a period: gateway traffic, imported
-- traffic, or the difference. Mirrors the shape of the existing dashboard
-- indexes, which are all (dimension, occurred_at DESC).
CREATE INDEX usage_event_origin_idx ON usage_event (origin, occurred_at DESC);
