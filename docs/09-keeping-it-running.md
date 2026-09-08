# Keeping it running

`docs/07-running-locally.md` ends at a request that works. This is what surfaces
afterwards — the questions a working gateway starts asking a week in, and the
ones an outage asks at 21:40. Each section is here because it cost somebody an
evening.

## The dev database is durable now, and `oag_g0` is not in it

`deploy/compose/dev.yml` mounts a **named volume**, so data on 127.0.0.1:5452
survives container restarts, Docker Desktop restarts and reboots. Only
`just dev-reset` (`down -v`) destroys it — which is what the justfile always
claimed, and is now true. Until 2026-09-07 it was `tmpfs`: the engine restarted,
the mount came back empty, and it took the schema, every credential, both API
keys and a dependent service's database with it, with nobody running a reset.

Two things follow.

**Redis is still deliberately disposable** (`--save "" --appendonly no`, no
volume) and should stay that way. Everything in it is derived or advisory: the
L2 auth cache refills from Postgres, sticky pins are affinity hints, concurrency
slots carry their own TTLs. A cold start costs a cold cache.

**`oag_g0` — the gated-test database — is hand-made and not in the compose
file.** After any `dev-reset` it must be recreated or every gated test silently
skips:

```bash
docker exec oag-dev-postgres-1 createdb -U oag oag_g0
```

Not the host `createdb -U oag`: it prompts for a password it cannot read when
`PGPASSFILE=/dev/null`, and loops until the server drops the connection. Point
`OAG_TEST_DATABASE_URL` at `oag_g0`, never at `oag` — the gated tests call
`migrate()` themselves and seed fixtures into whatever it names.

`oag_g0` is also **not clean**. Rows survive from every run that panicked before
its cleanup, so Postgres picks plans from populated statistics while CI picks
them from an empty table. A plan-shape or whole-table-count assertion can be
green here and red on CI for a reason unrelated to what it asserts.

## Moving the data

The host `pg_dump` is probably older than the server (14.x against 18.x aborts
on the version mismatch). Run it in the container:

```bash
docker exec oag-dev-postgres-1 pg_dump -U oag -d oag --no-owner --no-acl > oag.sql
psql "postgres://oag:oag@127.0.0.1:5452/oag" -v ON_ERROR_STOP=1 -f oag.sql
```

**`ON_ERROR_STOP=1` is not optional.** `psql -f` continues past errors by
default, so restoring into an already-migrated database fails every
`CREATE TABLE`, lands some `COPY`s anyway, and leaves a database that *looks*
migrated. A restore that refuses outright is strictly better than one that
half-succeeds.

Two things to know before you start. `just dev` is `dev-up migrate`, so it
migrates as a side effect and will collide with a restore — bring the containers
up alone with `docker compose -f deploy/compose/dev.yml up -d postgres redis`.
And `api_key` stores hashes, so keys survive a dump and restore: callers need no
new values, and the way to prove the restore worked is to compare hash digests,
not row counts.

## Ownership: why a credential can be invisible

`account add` requires `--owner-email <email>` or `--shared`, and the refusal is
deliberate — a personal subscription binds to one principal unless somebody
deliberately pools it.

The consequence is the part that bites. `repo::candidates` filters
`owner_principal_id IS NULL OR = $3`, so a bound credential is **invisible** to
another principal rather than refused. If a consuming service mints its own
per-user keys on its own principal, those users see no credential at all, and
the error says only "no credential for `<provider>`".

**There is no CLI path to change ownership afterwards.** `oag admin account` has
`add`, `rename`, `list`, `disable`, `enable`, `set-cost` and `set-reserve` — no
`delete`, no `share`, and `--shared` applies only at import. Pooling an existing
seat today means writing `account.owner_principal_id = NULL` directly. That gap
is worth a subcommand.

## Reading a "no credential" error

**On a route where nothing is visible, the error names the last rung tried, not
the thing that is missing.** One fault — two seats bound to a principal the
caller was not on — reported `anthropic`, then `openai`, then `xai` over one
evening as the ladder changed underneath it. A classified request names your
*ceiling*; a hard pin names the provider you pinned, because it has no ladder to
climb.

So read it as *"the search ended here"*, never *"this provider is broken"*. The
log line beside it carries what the message cannot:

```
no credential this caller can use on this route
  provider=… principal_id=… route_id=… candidates=0 reserve_holding_back=false
```

`candidates=0` with a principal id is the diagnosis — the search ran, as that
identity, and the route holds nothing that identity can use.
`candidates=3 reserve_holding_back=true` is a different sentence entirely.

A refused inbound key logs too, and logs two different things on purpose. An
unknown but correctly-shaped key logs its prefix, which is the same sixteen
characters `api_key.key_prefix` stores, so it joins to a row. A string that is
not key-shaped logs only that fact: its leading characters are somebody else's
secret, and the mistake this path exists to catch is a provider key pasted into
a client pointed here.

## Probe. Never trust a list

An advertised model is not a servable one, and the reverse. All three seen in
one week:

- **Servable but unadvertised.** `xai/grok-4.6` dispatches, but xAI's usage
  endpoint returns quota and no model list, so `served_models` stays empty and
  `/v1/models` never mentions it.
- **Advertised but unservable.** A Codex seat listed `gpt-5.5` in
  `served_models`; a request for it returns 404.
- **`served_models` is not a gate.** It feeds the savings baseline, not
  dispatch: *"a failure here degrades the savings baseline and must never fail
  the request"*.

The catalogue is a picker's list, never an authority. Probe by hard pin.

Prefer **two providers** on a ladder where you can. R1's climb past a dead
provider needs a rung on a different one, and a single-provider ladder simply
503s when its one credential is unavailable — which is how an all-Anthropic
`default` ladder with no Anthropic credential 503'd every `oag/*` pin here.

## The reserve

```bash
oag admin account set-reserve grok-seat --pct 10
```

Below 10% remaining the seat stops being scheduled and the ladder falls through
to the next rung, rather than the pool draining to nothing. It is evaluated
against `usage_remaining_pct`, which the usage poller refreshes — so it only
works where the provider reports one, and `usage_poll_interval: 0` disables the
poller and with it every reserve (the gateway warns at startup and serves
anyway). xAI reports a percentage on a weekly unified-billing account and not on
a monthly-only one, where the parser deliberately returns nothing rather than
mislabel a monthly figure as weekly.

A seat sitting at exactly `100.00` is a real measurement, not a placeholder: a
true 0% used on a large weekly pool, in a `numeric(5,2)` column.

## Points read `—` until an admin sets the reference

`points_reference` is empty on a fresh database, and until it is set every
multiplier and every points figure is `null`. That is correct behaviour, not a
fault, and it is why a dashboard's points column shows an em-dash.

Set it with `PUT /admin/api/points/reference` — **admin API only; there is no
CLI subcommand.** R is a *chosen* denominator that defines what one point is
worth (at R = $0.20/Mtok, a $5/Mtok model reads as 25×), not a fact to look up.
Points are derived at read time and stored nowhere, so changing R moves only
displayed figures and is reversible.

## Health

`/health/live` never checks a dependency. A liveness probe that fails during a
database outage restarts the fleet.

`/health/ready` checks Postgres, Redis **and the schema**. The schema check
exists because `SELECT 1` succeeds against a database with no tables: a replica
whose schema had vanished answered `{"ready":true,"database":true}` while every
request returned 500. It compares applied migrations against what the binary
carries — `>=`, not `==` — so a schema *ahead* of the binary (the middle of a
rolling deploy) stays ready and one behind does not.
