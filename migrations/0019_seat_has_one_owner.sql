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
-- A trigger, not a CHECK. Postgres checks a CHECK constraint — NOT VALID
-- included — on every UPDATE of a row, so a legacy owner-less seat could no
-- longer be disabled, renamed, priced, polled or have its rotated token
-- stored: every write to it would fail until someone bound it, and a refresh
-- that failed to store would drop the new token on the floor. The rule wanted
-- is narrower: no write may *produce* an owner-less seat that was not one
-- already. So it refuses an owner-less seat on INSERT, and on UPDATE refuses
-- clearing a seat's owner or turning an owner-less key into a seat, while an
-- already owner-less seat can still be written to. It stays inert either way:
-- the request path matches it for nobody, and the usage poller skips it.
CREATE FUNCTION account_seat_has_one_owner() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind = 'oauth' AND NEW.owner_principal_id IS NULL
       AND (TG_OP = 'INSERT'
            OR OLD.owner_principal_id IS NOT NULL
            OR OLD.kind <> 'oauth') THEN
        RAISE EXCEPTION 'account_seat_has_one_owner: a subscription seat belongs to one person; '
            'account % (%) needs an owner (oag admin account set-owner)', NEW.name, NEW.provider
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER account_seat_has_one_owner
    BEFORE INSERT OR UPDATE OF kind, owner_principal_id ON account
    FOR EACH ROW EXECUTE FUNCTION account_seat_has_one_owner();
