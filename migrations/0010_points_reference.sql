-- The points reference price: one row, the admin's. A point is one token at this price (USD per
-- million tokens), so a model's multiplier is its list price over this one, and a request's
-- points are its counterfactual_api_usd × 1,000,000 / usd_per_mtok — derived at read time and
-- never stored, so changing the price re-values every past figure alike and "N points" keeps
-- meaning "N reference tokens". A partner service (OpenGrok) enforces limits in points; this
-- gateway stays the meter, one enforcer per rule.
CREATE TABLE points_reference (
    only_row      boolean       PRIMARY KEY DEFAULT true CHECK (only_row),
    usd_per_mtok  numeric(12,6) NOT NULL CHECK (usd_per_mtok > 0),
    updated_at    timestamptz   NOT NULL DEFAULT now()
);
