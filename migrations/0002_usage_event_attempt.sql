-- Two ledger rows for one client request.
--
-- A quality gate can judge a cheap model's answer unusable and retry a rung up.
-- The abandoned attempt was still generated and invoiced by the provider, so it
-- needs its own row — but with `request_id` alone as the primary key the second
-- write conflicted with the first and `ON CONFLICT DO NOTHING` dropped it, so
-- one of the two attempts was simply absent from the ledger.
--
-- Keying on the attempt as well keeps both, and keeps both attributable to the
-- one request the client made.
ALTER TABLE usage_event ADD COLUMN attempt smallint NOT NULL DEFAULT 0;

ALTER TABLE usage_event DROP CONSTRAINT usage_event_pkey;
ALTER TABLE usage_event ADD PRIMARY KEY (request_id, attempt);
