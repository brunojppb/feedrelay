# Use Apalis with SQLite as the job queue from day one

A single-user, one-tap-at-a-time service does not need a durable job queue to function — a synchronous handler or a fire-and-forget tokio task would suffice for v1. We chose Apalis backed by SQLite anyway, because (a) restarts and redeploys mid-run should not lose work, (b) we want background workers as a foundational primitive available to future features (nightly cleanup, trip-aware dedup, multi-channel) without re-architecting, and (c) the queue lives in the same SQLite file as app data, so the operational overhead is "one more set of tables" rather than a new service.

## Consequences

- The `runs` table (app-owned) and Apalis's job tables coexist in the same DB. Two sources of truth for "is this run done?" — Apalis's job state is the authoritative scheduler view; the `runs` table is the domain audit. See [[ADR-0002]] (if recorded) for how they reconcile.
- Apalis migrations and app migrations must both run cleanly at boot. Migration ordering and ownership need to be explicit.
- The `/trigger/status/{run_id}` endpoint becomes mandatory (not optional) since the API is async by design.
