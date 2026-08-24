-- Subscription usage/quota monitoring.
--
-- OAG records what a subscription seat's traffic would COST (counterfactual_api_usd
-- on usage_event). It has had no idea how much of the seat's flat ALLOWANCE is
-- left — "80% of the weekly Grok pool is gone". A background poller reads each
-- provider's own usage endpoint and lands the answer here.
--
-- These columns are denormalised onto `account` rather than kept in a history
-- table: only the latest reading matters for both the dashboard and the
-- scheduler, and a poll every few minutes is not worth a row per reading.

ALTER TABLE account
    -- 0..100. The scarcer of what the provider reports; NULL until first polled,
    -- which is not the same as 0 — an unpolled seat's headroom is unknown, not
    -- full, and the dashboard says so.
    ADD COLUMN usage_remaining_pct numeric(5,2),
    -- Human label for the window the percentage is measured over, e.g. "weekly"
    -- — providers meter on different periods and the number is meaningless
    -- without it.
    ADD COLUMN usage_window_label  text,
    -- When the last successful poll ran. NULL = never. Drives a stale badge: a
    -- reading from an hour ago on a five-minute poll means polling is broken.
    ADD COLUMN usage_polled_at     timestamptz;

-- `window_resets_at` already exists (added in the baseline for the scheduler's
-- use-it-or-lose-it stage) and was populated by nothing in production. The
-- poller now fills it from the subscription's real reset time, so a seat whose
-- weekly pool is about to refresh is preferred while its unused capacity would
-- otherwise evaporate. No schema change needed for it here.
