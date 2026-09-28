-- A subscription seat belongs to exactly one person.
--
-- `owner_principal_id` NULL used to mean "the shared pool" for every kind. For
-- an API key that is what the providers permit: a console key pooled for an
-- organisation's own users. For a subscription seat (`oauth`: a ChatGPT/Codex
-- or SuperGrok plan) it is one person's plan serving several people, which is
-- what those plans' terms forbid and what got an account banned. One person may
-- own several seats; a seat never serves anyone but its owner.
--
-- The request path already treats an owner-less seat as matching no one
-- (`repo::candidates`, `route_channels`, `route_channel_status`). This makes the
-- schema refuse to create one.
--
-- NOT VALID: the rule binds every row written from now on, but an existing
-- owner-less seat does not stop the gateway from booting. It is inert already,
-- `oag admin doctor` lists it, and `oag admin account set-owner` binds it.
-- Once none are left:
--
--     ALTER TABLE account VALIDATE CONSTRAINT account_seat_has_one_owner;
ALTER TABLE account
    ADD CONSTRAINT account_seat_has_one_owner
    CHECK (kind <> 'oauth' OR owner_principal_id IS NOT NULL) NOT VALID;
