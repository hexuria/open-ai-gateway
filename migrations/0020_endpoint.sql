-- Endpoints: upstreams an operator registers, rather than ones built into the
-- code.
--
-- Until now the gateway could reach only the providers compiled into it, each
-- at one base URL for the whole deployment. An endpoint has four parts: a name,
-- which is its identity and its model prefix (`groq/llama-...`); a dialect,
-- which decides the wire codec; a platform, which decides the URL scheme and
-- how a request is signed; and its settings. Nothing else needs a new column.
-- `account.provider` and `model_catalog.provider` are free text, so a key for
-- an endpoint is an ordinary sealed `account` row whose provider is the
-- endpoint's name, and its models are ordinary catalog rows.
--
-- So this table holds no secret, and must never be made to hold one.
-- `extra_headers` is for headers that carry no authority, such as an
-- organisation id or a beta flag. A key pasted there would sit in the clear in
-- every backup, which is the thing sealing the `account` row exists to prevent.
--
-- Expand-only. The previous release does not read this table, so adding it
-- while that release is still serving is safe.
--
-- The CHECKs are the second line, as they are for `service`: the CLI and the
-- admin API validate first, and these hold for whatever reaches the database
-- without going through them. The main one is the platform matrix. A pair
-- outside it has no code that could serve it, so it cannot be stored:
--
--   plain   openai, anthropic, gemini, system_one   base_url required
--   azure   openai                                  base_url required
--   aws     anthropic, bedrock_converse             region required
--   gcp     gemini, anthropic                       region and project required
--
-- `base_url` may be NULL only on aws and gcp, where the host is derived from
-- the region; no other platform has a host without one. It carries no query
-- string and no fragment. Where an upstream wants a query (Azure's
-- `api-version`), the code builds it, and a stored URL that already had one
-- would be two URLs glued together.
--
-- `account.kind` admits `service_account` again. 0018 removed it because no
-- adapter had ever served one. A Google Vertex endpoint (platform gcp)
-- authenticates with a service-account JSON key, sealed in an `account` row of
-- this kind, which the gateway exchanges for short-lived bearer tokens. The
-- adapter that does the exchange comes later. Until it lands, nothing this
-- release ships writes such a row: `CredentialKind` does not parse the kind, so
-- `oag admin account add` refuses it. 0019's `account_seat_has_one_owner` is
-- unaffected, because it asks only `oauth` rows for an owner, and a service
-- account is an organisation's credential, pooled the way an API key is.
-- `account_claude_subscription_never_serves` names `oauth` alone and is
-- untouched too.
--
-- An endpoint that a credential or a catalog row still names must not be
-- removed out from under them, so `repo::delete_endpoint` refuses while any
-- row names it. It counts them under a `FOR UPDATE` lock on the endpoint row,
-- and that lock alone would not be enough. An INSERT into `account` never
-- touches the endpoint row, so a credential written between the count and the
-- commit would get past the count unseen. A foreign key would close that, but
-- these columns cannot have one, because they also name built-in providers,
-- which have no row here. `endpoint_reference_holds` does what the foreign key
-- would have done. Any write that sets `account.provider` or
-- `model_catalog.provider` to an endpoint's name takes `FOR KEY SHARE` on that
-- endpoint's row until it commits:
--
--   - If the delete locks first, the write waits. It is then refused, because
--     the row it named is gone.
--   - If the write locks first, the delete waits, and its count then sees
--     what was written.
--
-- The lock is a key-share lock, so `update_endpoint` blocks nobody. A write
-- naming no endpoint (a built-in provider, or a name nobody has registered)
-- costs one primary-key probe and takes no lock.

CREATE TABLE endpoint (
    -- The identity and the model prefix. The code applies the same pattern and
    -- also refuses the built-in providers' names, which this table cannot know.
    name            text        PRIMARY KEY
                    CONSTRAINT endpoint_name_check
                    CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,31}$'),
    dialect         text        NOT NULL
                    CONSTRAINT endpoint_dialect_check
                    CHECK (dialect IN (
                        'openai', 'anthropic', 'gemini', 'system_one', 'bedrock_converse'
                    )),
    platform        text        NOT NULL DEFAULT 'plain'
                    CONSTRAINT endpoint_platform_check
                    CHECK (platform IN ('plain', 'aws', 'gcp', 'azure')),
    base_url        text,
    -- How the key reaches a `plain` upstream. The other platforms sign their
    -- own way (SigV4, a minted bearer token, Azure's `api-key` header).
    auth            text        NOT NULL DEFAULT 'bearer'
                    CONSTRAINT endpoint_auth_check
                    CHECK (auth IN (
                        'bearer', 'x_api_key', 'x_goog_api_key', 'api_key_header', 'none'
                    )),
    region          text,
    project         text,
    api_version     text,
    extra_headers   jsonb       NOT NULL DEFAULT '{}'
                    CONSTRAINT endpoint_extra_headers_check
                    CHECK (jsonb_typeof(extra_headers) = 'object'),
    display_name    text,
    discover_models boolean     NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT endpoint_platform_dialect_check CHECK (
           (platform = 'plain' AND dialect IN ('openai', 'anthropic', 'gemini', 'system_one'))
        OR (platform = 'azure' AND dialect = 'openai')
        OR (platform = 'aws'   AND dialect IN ('anthropic', 'bedrock_converse'))
        OR (platform = 'gcp'   AND dialect IN ('gemini', 'anthropic'))
    ),
    CONSTRAINT endpoint_base_url_check CHECK (
        CASE WHEN base_url IS NULL THEN platform IN ('aws', 'gcp')
             ELSE base_url ~ '^https?://' AND base_url !~ '[?#]'
        END
    ),
    CONSTRAINT endpoint_region_check
        CHECK (platform NOT IN ('aws', 'gcp') OR region IS NOT NULL),
    CONSTRAINT endpoint_project_check
        CHECK (platform <> 'gcp' OR project IS NOT NULL)
);

COMMENT ON COLUMN endpoint.extra_headers IS
    'Non-secret headers sent on every request to this endpoint, stored in the '
    'clear. Never a credential: keys live sealed in account rows.';

ALTER TABLE account DROP CONSTRAINT account_kind_check;
ALTER TABLE account
    ADD CONSTRAINT account_kind_check
    CHECK (kind IN ('api_key', 'oauth', 'bedrock', 'service_account'));

CREATE FUNCTION endpoint_reference_holds() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Read before locking. A row that disappears while this waits for its lock
    -- then looks different from a row that was never there, which is the
    -- difference between a refused write and a built-in provider.
    IF EXISTS (SELECT 1 FROM endpoint WHERE name = NEW.provider) THEN
        PERFORM 1 FROM endpoint WHERE name = NEW.provider FOR KEY SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'endpoint_reference_holds: endpoint % was removed while '
                'this % row naming it was being written', NEW.provider, TG_TABLE_NAME
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER endpoint_reference_holds
    BEFORE INSERT OR UPDATE OF provider ON account
    FOR EACH ROW EXECUTE FUNCTION endpoint_reference_holds();

CREATE TRIGGER endpoint_reference_holds
    BEFORE INSERT OR UPDATE OF provider ON model_catalog
    FOR EACH ROW EXECUTE FUNCTION endpoint_reference_holds();
