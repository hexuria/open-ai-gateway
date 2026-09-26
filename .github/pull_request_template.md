## What and why



## Verification impact

- [ ] Pure Rust deterministic behavior
- [ ] Concurrency / interleaving (slots, leases, heartbeats, breakers)
- [ ] Crash / recovery (Redis or Postgres outage, restart, failover)
- [ ] Persistence semantics (migrations, store queries)
- [ ] Request/response dialect translation
- [ ] Dependencies or CI
- [ ] No verification architecture impact

Reason:
Affected invariants:
Tests updated:
