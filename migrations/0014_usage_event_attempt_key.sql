-- The contract half of the ledger's identity change. 0003 was the expand half.
--
-- 0003 added `attempt` and a unique index on `(request_id, attempt)`, and said
-- what it was waiting for: "A later release contracts by dropping the primary
-- key alone, with no code change needed on either side of that deploy." This is
-- that release, and the reason to do it now is that two of the ledger's own
-- documented rows do not exist without it.
--
-- One client request can pay for several attempts. A quality gate can abandon a
-- cheap answer and retry a rung up; a credential can generate an answer and
-- lose the stream before another credential serves the retry. Every one of
-- those was generated and invoiced by a provider. Under a primary key of
-- `request_id` alone, only the first row for a request survives, so the second
-- attempt's row is dropped by `ON CONFLICT DO NOTHING` and its spend leaves the
-- ledger. `record_abandoned` works around that by writing after the served row,
-- which means the abandoned row has never once landed.
--
-- Why it is safe to drop the key now, which it was not in 0003:
--
--   * 0003's hazard was a previous release whose metering named
--     `ON CONFLICT (request_id)`. Dropping the key out from under it would fail
--     every insert with 42P10 for the whole rolling-deploy window. No live
--     release does that any more — both this one and the one before it use an
--     untargeted `ON CONFLICT DO NOTHING`, which arbitrates against whichever
--     unique constraint exists. The same is true of the importer's insert.
--   * Nothing references `usage_event` by foreign key.
--   * No query assumes one row per request. The admin usage listing selects
--     `request_id` as a column, and showing both attempts of an escalated
--     request is the answer that page exists to give.
--   * `WHERE request_id = $1` keeps an index: the promoted key leads with
--     `request_id`, so it serves a prefix lookup exactly as the old one did.
--
-- `USING INDEX` promotes the index 0003 already built rather than rebuilding
-- it, so this takes a brief lock and no scan. Postgres renames the promoted
-- index to `usage_event_pkey`; the name `usage_event_request_attempt_key` does
-- not survive, which is why the test that named it is retired in this release.
ALTER TABLE usage_event DROP CONSTRAINT usage_event_pkey;

ALTER TABLE usage_event
    ADD CONSTRAINT usage_event_pkey
    PRIMARY KEY USING INDEX usage_event_request_attempt_key;

COMMENT ON COLUMN usage_event.attempt IS
    'Which dispatch of this request the row accounts for, counted from zero '
    'across both loops that dispatch: escalation up the ladder and credential '
    'failover within a rung. Half of the primary key since 0014.';
