-- Seat-honest accounting.
--
-- A subscription seat (kind='oauth') is flat-rate: the tokens are paid for by
-- a monthly fee, so a per-request `cost_usd` of zero is the truth, not an
-- absence of data. But zero cost against a full `counterfactual_usd` would make
-- the frontier savings figure treat a seat as infinitely cheap, and it would
-- make the seat's own value — the pay-per-token API bill it displaced —
-- invisible. Two columns fix that without disturbing the money the ledger
-- reconciles against an invoice.

-- What this row's tokens would cost at the *same model's* list API price. For a
-- metered account this equals `cost_usd`, so a SUM over mixed traffic stays
-- meaningful; for a seat it is the pay-per-token bill the subscription avoided.
-- The subscription's worth is SUM(counterfactual_api_usd - cost_usd) minus the
-- prorated seat fee below.
ALTER TABLE usage_event
    ADD COLUMN counterfactual_api_usd numeric(14,8) NOT NULL DEFAULT 0;

-- The flat monthly price of a seat, so the saving it books can be netted
-- against what it costs. NULL for metered credentials, which have no fixed fee
-- — and NULL is not zero here: a seat whose price nobody recorded should not
-- read as free money saved.
ALTER TABLE account
    ADD COLUMN monthly_cost_usd numeric(10,2);
