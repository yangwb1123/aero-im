Review complete. Written to `docs/pi-batch/sdlc-reviews/database_architect.md`.

## What I did

Applied the `database_architect.md` role prompt (found at `/home/u1/ai-batch-runner/prompts/`; there is no `prompts/README.md` inside this repo — I applied the shared rules from the runner's copy). Independently re-verified **every** load-bearing DB claim in the design's verification ledger against the tree, then ran the required store-inventory + hot/atomic-path + migration analysis.

## Verification results

All design claims (V1–V14, F1–F7) **reproduced**, with two strengthened beyond the ledger:

- **App-clock discipline holds table-wide**: I enumerated *every* `INSERT INTO messages` in the tree — the only production insert (`crud.rs:79`) binds app `OffsetDateTime`; all 15 DB-clock (`now()`/DEFAULT) inserts are `#[cfg(test)]` fixtures. F5's parameter-bound backdate pattern is the correct one.
- **sqlx 0.8.6 `Migrator` takes a per-DB `pg_advisory_lock`** (`migrate.rs:182`) — concurrent boot migrations serialize; 0238's DROP+CREATE index reissue is safe correctness-wise.

## Store inventory (hot vs durable, stock wiring)

- **Durable & wired**: PostgreSQL (system of record + `event_outbox` transactional outbox with strict per-message aggregate ordering), NATS JetStream (`im.room.*` durable), blob store (LocalFs/S3 fail-loud).
- **Hot & wired**: Redis (presence/rosters/rate — fail-open, AOF in compose but never relied on), in-process Hub/rate buckets/SfuRouter.
- **Dormant, NOT wired**: `messages_partitioned` (0148 partition shadow — runbook-only cutover; binary only mirrors schema, GDPR-deletes, exposes backfill fn). No phantom/orphaned backend found.

## Findings (no Critical/High)

- **F1 [Low]** silent-fallback env knob → recommend one-line effective-window log at `ImService::new` (ops can't diagnose a typo silently becoming 86400).
- **F2 [Info]** metric counts preflight rejections only; boundary-race fraction uncounted — document semantics.
- **F3 [Low, pre-existing]** `list_since` (WS reconnect) has no full `(room_id, id)` index — the only one is partial `WHERE deleted_at IS NULL` (0075), unusable because the query includes tombstones. Scale risk, not a blocker.
- **F4–F7 [Verified]** `changes_since` index covers recall; zero-migration claim holds; race partition sound with zero new lock surface (window check reads the already-`FOR UPDATE`-locked row).

The recall tx stays ~13 statements with a pure comparison inserted at step 8 — no new queries, locks, or deadlock ordering. Migration sequence: code-only, symmetric rollback, with validation queries and shadow-parity check included. Volume/recovery gaps (row counts, EXPLAIN ANALYZE, PG backup drill) marked Unknown with the measurements required.
