-- A floor under a subscription seat's quota.
--
-- Until now the only thing that took a seat out of rotation was the provider
-- saying no: the gateway spent the weekly pool down to nothing, collected a
-- 429, and every user of that seat was blocked until the window reset. The
-- remaining percentage has been polled and stored since 0005 and nothing acted
-- on it. This is the column that acts on it.
--
-- Nullable rather than defaulted to zero. NULL means "no reserve", which is
-- exactly today's behaviour, so an existing fleet upgrades without any seat
-- changing how it schedules. A default would silently impose a policy nobody
-- asked for on every credential at once.
--
-- Expand-only: the previous release neither reads nor writes this column, so it
-- runs unchanged against the new schema and a rollback loses the reserves and
-- nothing else.

ALTER TABLE account
    -- 0..100. Once `usage_remaining_pct` has fallen to or below this, the seat
    -- stops being scheduled until its window resets. Compared against a reading
    -- the provider produced, so a NULL reading is never treated as exhausted:
    -- an unpolled seat's headroom is unknown, and benching a working credential
    -- because nobody measured it is a worse failure than the one this prevents.
    ADD COLUMN usage_reserve_pct smallint
        CHECK (usage_reserve_pct IS NULL OR usage_reserve_pct BETWEEN 0 AND 100);
