# Service catalog

OAG is the organisation's model door: auth, budget, cost routing, dialect
hub, credential pool. A capability service — a sandbox, a tool host, a
guard, a reducer, a browser — is a different process with its own
dashboard. This catalog is how the gateway sits *on top of* those
services without becoming them.

Register a row. Health-check it. Deep-link to the service's own UI. That
is the whole slice.

## What a row is

| Field | Meaning |
|---|---|
| `name` | Operator-facing label. Unique. |
| `kind` | `sandbox`, `tool`, `guard`, `reduce`, `harness`, `browser`, or `other`. |
| `base_url` | http(s) only. Where the service lives. |
| `health_path` | Joined onto `base_url`. Must be a path (`/health`), not a second URL. |
| `dashboard_url` | Optional. Opened in a new tab. OAG does not render that UI. |
| `auth_ref` | Optional pointer at an existing `account` row. Not a second vault. |
| `enabled` | Soft disable. The row stays; probes and deep-links still exist. |
| `last_ok` / `last_error` | Outcome of the last health GET. |

The gateway does not implement the capability. It does not start a
microVM, open a VNC session, parse a PDF, or run a guardrail. Those
belong to the service you pointed at.

## What it will not become

Panday, Berthos, Headroom, and Orgo are examples of *backends*. They are
not templates to copy into this repository. An adapter that speaks
Firecracker, CodeSandbox, or AI-Infra-Guard is a later decision, and a
different crate, if it happens at all.

A generic key/value "settings" table is also not this. Each column is
typed because a catalog that accepts anything is how a gateway turns
into a monolith one reasonable-looking field at a time.

## Health and SSRF

`POST /admin/api/services/{id}/check` GETs `base_url + health_path` with
a short timeout and no redirects. Create and update probe once so the
row is not born with an unknown health.

The check fails closed on:

- any scheme other than `http` or `https`
- URLs with embedded credentials
- link-local addresses (`169.254.0.0/16`, `fe80::/10`) and the
  well-known cloud-metadata hostnames
- a resolved address that lands on one of those, after DNS

Loopback and RFC1918 are allowed: that is where the organisation's own
services actually run. The catalog is an internal registration surface,
not a crawler.

## Admin API

All of these sit behind the existing admin-key layer.

| | |
|---|---|
| `GET /admin/api/services` | List, including stored health. |
| `POST /admin/api/services` | Create. Body is the catalog fields. |
| `PATCH /admin/api/services/{id}` | Replace catalog fields. |
| `POST /admin/api/services/{id}/disable` | Soft disable. |
| `POST /admin/api/services/{id}/enable` | Put it back. |
| `POST /admin/api/services/{id}/check` | Probe now and store `last_ok` / `last_error`. |
