-- Credential kinds this build can actually serve, and no Claude subscription
-- that can serve.
--
-- 0001 allowed `vertex` and `service_account` in `account.kind`. Neither ever
-- had an adapter: a row of either kind could be registered and could not serve,
-- and the code that carried them (the `Provider::Vertex` and
-- `CredentialKind::Vertex`/`ServiceAccount` variants) is gone. The column now
-- admits exactly the kinds some provider offers.
--
-- A Claude.ai / Claude Code subscription must never serve a request through
-- this gateway (docs/compliance.md). It may still exist as a row for one
-- reason: `oag admin usage import --account <plan>` books Claude Code traffic
-- the user ran directly against the plan that paid for it, so the savings
-- report can say what that fee covered. That row serves nothing and needs no
-- token. The rule the schema can state is therefore not "no such row" but "no
-- such row that can be leased": an `anthropic` row of kind `oauth` is never
-- `schedulable`. The kind CHECK knows kinds, not pairs, so it never said this.
--
-- A `vertex`/`service_account` row stops the migration: it is an operator's to
-- remove, and a silent DELETE here is the wrong place to learn one existed. A
-- schedulable Claude subscription is switched off instead of refused, because
-- switching it off is exactly the protection this migration exists to give,
-- and it loses nothing.

DO $$
DECLARE
    dead bigint;
    claude bigint;
BEGIN
    SELECT count(*) INTO dead FROM account WHERE kind IN ('vertex', 'service_account');
    IF dead > 0 THEN
        RAISE EXCEPTION
            '% account row(s) of kind vertex/service_account exist. No adapter ever served '
            'them. Remove them (SELECT name, provider, kind FROM account WHERE kind IN '
            '(''vertex'',''service_account''); then DELETE) and restart.', dead;
    END IF;

    UPDATE account SET schedulable = false
     WHERE provider = 'anthropic' AND kind = 'oauth' AND schedulable;
    GET DIAGNOSTICS claude = ROW_COUNT;
    IF claude > 0 THEN
        RAISE NOTICE
            '% Claude subscription account row(s) were schedulable and are now not: '
            'they may book imported usage but never serve (docs/compliance.md).', claude;
    END IF;
END
$$;

ALTER TABLE account DROP CONSTRAINT account_kind_check;
ALTER TABLE account
    ADD CONSTRAINT account_kind_check CHECK (kind IN ('api_key', 'oauth', 'bedrock'));

ALTER TABLE account
    ADD CONSTRAINT account_claude_subscription_never_serves
    CHECK (NOT (provider = 'anthropic' AND kind = 'oauth' AND schedulable));
