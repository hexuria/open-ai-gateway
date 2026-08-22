# Modules

Every `data-*` module exposes the same two outputs:

| Output | Meaning |
|---|---|
| `database_url` | a `postgres://` URL the gateway can connect to |
| `redis_url` | a `redis://` or `rediss://` URL |

That is the whole interface. From the gateway's point of view a managed Cloud
SQL instance, an RDS instance, and a Neon branch are the same thing — a URL —
which is what makes the data tier swappable without touching the compute tier.

Both are marked `sensitive`, so they do not appear in plan output.

Every `compute-*` module takes those two URLs plus an image reference, and
exposes `url` — where the gateway can be reached.
