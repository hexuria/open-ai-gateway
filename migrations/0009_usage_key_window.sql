-- A key's spend over a window: the per-coworker limits a partner service (OpenGrok) evaluates
-- before each model call — a rolling five hours, a rolling seven days, the calendar month —
-- are sums of this key's ledger rows since an instant. Every other usage index leads with a
-- different column; without this one each such read walks the key's whole history.
CREATE INDEX IF NOT EXISTS usage_event_key_idx ON usage_event (api_key_id, occurred_at DESC);
