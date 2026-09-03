-- What a credential can actually be used with, as the provider told us.
--
-- Visibility used to be decided by ladder membership: `on_offer` offered any
-- model on a rung and dropped every off-ladder catalogue row unless the route
-- held a metered key. On a subscription-only route that made the ladder the
-- only thing making a model visible, which is why a seat advertised models it
-- refuses and hid models it serves. Neither the catalogue nor a proxy's model
-- list can answer this: the served set is an entitlement of the PLAN behind
-- one credential, so two seats at the same provider can differ, and the only
-- authority is the credential itself.
--
-- NULL means never asked, and is deliberately different from an empty array.
-- Unknown falls back to the old ladder rule so a gateway that has not yet
-- discovered anything keeps working; empty is the provider positively saying
-- it serves nothing, which hides the credential's models.
ALTER TABLE account
    ADD COLUMN served_models    text[],
    ADD COLUMN served_models_at timestamptz;

COMMENT ON COLUMN account.served_models IS
    'Upstream model names this credential serves, as the provider stated them. '
    'NULL = never discovered (fall back to ladder visibility); '
    'empty = asked, serves nothing.';
