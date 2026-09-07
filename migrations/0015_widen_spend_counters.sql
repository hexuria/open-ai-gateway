-- The denormalised spend counters carry the ledger's own scale.
--
-- `usage_event.cost_usd` is `numeric(14,8)`. The three counters that mirror
-- sums of it — `api_key.spent_usd`, `principal.spent_usd`, `route.spent_usd` —
-- were `numeric(14,6)`, so every debit was rounded to six places on the way in.
--
-- Two decimal places sounds like nothing until you notice what these columns
-- are for. A cheap-rung request costs on the order of $0.0001, and the rungs
-- these counters exist to protect are chosen by comparing them against a cap.
-- At six places a request costing $0.000004 debits $0.000004 correctly, but one
-- costing $0.0000004 debits nothing at all — and a seat-served request, whose
-- `cost_usd` is truthfully zero, is not the only row that can round to zero.
-- Traffic made of such rows spends real money against a cap that never moves.
--
-- The drift is systematic rather than random: `ROUND` is half-up, but the
-- costs are not symmetric about a rounding boundary, and the same models are
-- billed the same way every time. So the counter and `SUM(cost_usd)` disagree
-- by an amount that grows with traffic, in a direction that depends on the
-- price list. 0012 introduced these counters precisely so a budget could be
-- enforced against a number that cannot be stale; a number that cannot be stale
-- but can be wrong is a smaller improvement than intended.
--
-- Precision goes 14 -> 16 alongside scale 6 -> 8 so the integer range is
-- unchanged: both allow eight digits before the point, which is $99,999,999.
-- Widening a numeric's precision and scale rewrites no rows and takes no table
-- lock beyond the catalogue update in recent Postgres, because the stored
-- representation of `numeric` is already variable-width.
--
-- Nothing rounds on the way out: `rust_decimal::Decimal` holds both scales, and
-- the reconciler compares the counter against the ledger sum for equality.
-- Before this it was comparing a six-place number with an eight-place one and
-- correcting the difference on every pass, for as long as the gateway ran.

ALTER TABLE api_key   ALTER COLUMN spent_usd TYPE numeric(16,8);
ALTER TABLE principal ALTER COLUMN spent_usd TYPE numeric(16,8);
ALTER TABLE route     ALTER COLUMN spent_usd TYPE numeric(16,8);

COMMENT ON COLUMN api_key.spent_usd IS
    'Lifetime spend, the denormalised sum of this key''s usage_event.cost_usd. '
    'Same scale as the ledger since 0015, so the two can be compared for '
    'equality rather than within a tolerance.';
COMMENT ON COLUMN principal.spent_usd IS
    'Month-to-date spend, the denormalised sum of this principal''s '
    'usage_event.cost_usd for the current month. Same scale as the ledger '
    'since 0015.';
COMMENT ON COLUMN route.spent_usd IS
    'Month-to-date spend, the denormalised sum of this route''s '
    'usage_event.cost_usd for the current month. Same scale as the ledger '
    'since 0015.';
