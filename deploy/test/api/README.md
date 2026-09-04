# Poking the API by hand

[Hurl](https://hurl.dev) files covering every endpoint this gateway serves, with
several examples each — one per way the endpoint is actually used, not one per
route — and `./api`, which lists, searches, inspects and sends them.

```sh
brew install hurl            # or: cargo install hurl
just api-keys                # mints two keys into .dev-hurl-vars (gitignored)

just api list                # every endpoint, numbered
just api list models         # only the ones about models
just api show 63             # doc, auth, assertions, request body
just api run 63              # send it
just api test                # every safe file, as a suite
```

`./deploy/test/api/api` is the same tool without `just` in front, and takes the
same arguments.

## The numbers are stable

`api list <term>` filters the view but keeps each endpoint's number from the
FULL list, so a number you read after a search is the number `run` wants. The
first version renumbered per search, which meant `api search points` showed a
`14` that `api run 14` resolved to something else entirely.

## The list is parsed, not maintained

`api` reads the `.hurl` files. There is no second copy of the routes to keep in
step — a list that must agree with something else, with no mechanism forcing it
to, drifts, and every drifting list this deployment has met drifted silently.
Add a request to a `.hurl` file and it appears in `api list` with no other edit.

## Running one request that depends on an earlier one

Some requests use a variable an earlier request in the same file captured — a
key id, a principal's email. `api run` notices, runs the requests up to that
point, and says so. On a file under `mutating/` that means earlier side effects
happen again, which it also says.

## Why these are files and not shell

A request in a `.hurl` file is the request. In a shell recipe it is a string
inside a string: the JSON is quoted for the shell, the shell is quoted for the
recipe, and the language you are actually writing is escaping. Three bugs in the
first hour of the `just`-based version were quoting or argument-order mistakes
rather than anything about the gateway, and none of them were visible by reading
the recipe.

Hurl also asserts. A recipe that prints a body tells you it answered; a file
with `[Asserts]` tells you it answered *correctly*, which is the difference
between poking an API and testing one.

## Layout

Safe — run by `just api`, 74 requests:

| file | reqs | what it covers |
|---|---|---|
| `health.hurl` | 5 | liveness on both listeners, readiness, metrics, the SPA |
| `discovery.hurl` | 7 | both listing spellings, the Gemini envelope, the `oag` diagnosis, the Claude Code alias toggle |
| `chat-completions.hurl` | 8 | OpenAI dialect: system prompt, multi-turn, tools, streaming, `@sub`, both paths |
| `messages.hurl` | 7 | Anthropic dialect, content blocks, `count_tokens` |
| `responses.hurl` | 6 | Responses dialect: string input, message list, instructions |
| `gemini.hurl` | 6 | model and action in one path segment, `generationConfig` |
| `errors.hurl` | 10 | 401/403/400/404/503 — every one observed, not guessed |
| `admin-read.hurl` | 17 | every admin GET, ids captured rather than hardcoded |
| `admin-points.hurl` | 8 | reference price, multipliers, the pool batch read |

State-changing — opt-in only:

| file | reqs | notes |
|---|---|---|
| `mutating/keys.hurl` | 9 | mint → use → quota → revoke → prove it is dead. **Self-cleaning** |
| `mutating/catalog.hurl` | 7 | label a model and put it back. **Reversible** |
| `mutating/points-reference.hurl` | 9 | writes the same value back. **No-op by design** |
| `mutating/principals.hurl` | 6 | leaves one principal |
| `mutating/services.hurl` | 9 | leaves one service per run |
| `mutating/accounts.hurl` | 9 | **needs `-V account_id=<uuid>`**. Named without one it errors; swept up by `--all` it is skipped with a notice |

## `mutating/` is separate on purpose

Those files revoke keys, disable credentials, rewrite the points reference price
and edit catalogue rows. The reference price in particular re-values every pool
and cap the deployment holds, so it is not something a suite should touch
because it happened to be in the directory.

Run one deliberately:

```sh
just api test mutating/keys
just api test mutating/keys -v          # every request and response in full
just api run 91                         # or a single request from it
```

`accounts.hurl` goes further and refuses to run without an id you chose:

```sh
just api test mutating/accounts -V account_id=<uuid>
```

Disabling a seat takes it out of rotation for real. On a deployment whose only
serving credential is that seat, a turn landing between the disable and the
enable fails for a reason nobody will connect to a test run.

After running `services.hurl` or `principals.hurl`, clear what they left:

```sh
psql "$OAG_DATABASE__URL" -c "delete from service where name like 'hurl-test-%'"
```

## Variables

`.dev-hurl-vars` holds `host`, `admin_host`, `api_key` and `admin_key`. It is
gitignored and no recipe prints it: a key is shown once at creation and only its
SHA-256 is stored, so the file is the only copy.

Override anything per run:

```sh
just api test chat-completions -m xai/grok-4.6
just api run 26 -V model=xai/grok-4.6 -V host=http://staging:8080
```
