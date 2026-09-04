-- Month-to-date spend, kept on the row it caps.
--
-- Two caps were enforced from two different kinds of wrong. The principal's
-- monthly budget was read from the auth cache, which snapshots a SUM over the
-- ledger at load and then holds it for five minutes: N concurrent requests all
-- saw the same stale figure, all found the wall not yet reached, and all went
-- through. The route's monthly budget was the opposite mistake — recomputed
-- from the ledger on every request, a month-wide SUM over usage_event that
-- grew all month, on the hot path, for the ordinary act of setting a budget.
--
-- api_key.spent_usd already had the right shape: a column the ledger write
-- increments in the same statement as the insert, so the cap check is one
-- primary-key read that is never behind the ledger. This gives the principal
-- and the route the same column, plus the month it refers to, so the ledger
-- write can reset it at the boundary rather than a scheduled job having to.
--
-- Backfilled from the ledger for the current month, once, here. From now on
-- record_usage maintains all three in one statement with the row insert.
ALTER TABLE principal
    ADD COLUMN spent_usd   numeric(14,6) NOT NULL DEFAULT 0,
    ADD COLUMN spent_month date;

ALTER TABLE route
    ADD COLUMN spent_usd   numeric(14,6) NOT NULL DEFAULT 0,
    ADD COLUMN spent_month date;

UPDATE principal p
   SET spent_usd = COALESCE((
           SELECT SUM(u.cost_usd) FROM usage_event u
            WHERE u.principal_id = p.id
              AND u.occurred_at >= date_trunc('month', now())
       ), 0),
       spent_month = date_trunc('month', now())::date;

UPDATE route r
   SET spent_usd = COALESCE((
           SELECT SUM(u.cost_usd) FROM usage_event u
            WHERE u.route_id = r.id
              AND u.occurred_at >= date_trunc('month', now())
       ), 0),
       spent_month = date_trunc('month', now())::date;

COMMENT ON COLUMN principal.spent_usd IS
    'Spend in the month named by spent_month, maintained by record_usage. '
    'Read as zero when spent_month is not the current month.';
COMMENT ON COLUMN principal.spent_month IS
    'First day of the month spent_usd covers. NULL = never spent.';
COMMENT ON COLUMN route.spent_usd IS
    'Spend in the month named by spent_month, maintained by record_usage. '
    'Read as zero when spent_month is not the current month.';
COMMENT ON COLUMN route.spent_month IS
    'First day of the month spent_usd covers. NULL = never spent.';
