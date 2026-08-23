-- Room in the ledger for a second attempt. The expand half of expand/contract.
--
-- A quality gate can judge a cheap model's answer unusable and retry a rung up.
-- The abandoned attempt was still generated and invoiced by the provider, so it
-- needs its own row — which means the ledger's identity has to become
-- (request_id, attempt) rather than request_id alone.
--
-- It does NOT become that here. During a rolling deploy the previous release is
-- still serving, and its metering says `ON CONFLICT (request_id)`. Dropping the
-- primary key out from under it makes every one of those inserts fail with
-- 42P10 — for the whole overlap window, and again for as long as a rollback
-- lasts. Losing quota writes to save a release is the wrong trade.
--
-- So this release only adds: the column, and a unique index wide enough for the
-- new key to be inferred against. The primary key on request_id survives, which
-- means a second row for one request is still dropped for one release cycle.
-- That is the ordinary price of expand/contract, and it is strictly better than
-- erroring. A later release contracts by dropping the primary key alone, with
-- no code change needed on either side of that deploy.
ALTER TABLE usage_event ADD COLUMN attempt smallint NOT NULL DEFAULT 0;

-- Not CONCURRENTLY: migrations run inside a transaction and CONCURRENTLY cannot.
-- Building this takes a brief exclusive lock on the ledger, which is the cost of
-- keeping the migration to one reviewable transaction.
CREATE UNIQUE INDEX usage_event_request_attempt_key ON usage_event (request_id, attempt);
