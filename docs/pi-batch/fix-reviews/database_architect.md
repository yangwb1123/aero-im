Review written to `docs/pi-batch/reviews/database_architect.md`. Summary:

## Store inventory (durability × stock wiring)

| Store | Durability | Stock binary wiring |
|---|---|---|
| PostgreSQL 17 | **Durable** source of truth (238 migrations, compile-time embedded, migrate-before-serve) | **Verified** `boot/persistence.rs` |
| NATS JetStream | **Durable** streams (7d IM_MESSAGES w/ dup-window=retention, 6h LIVE_EVENTS); at-least-once + poison bounds | **Verified** bootstrap + durable consumers/bots in `background.rs` |
| Redis 7 | **Hot/ephemeral** (presence/viewers/rate-windows); seq counters need persistence | **Verified**; compose runs `--appendonly yes` |
| Blob (LocalFs/S3/Vault) | **Durable**; `AERO_BLOB_BACKEND=s3|vault` fails boot loud (no silent local fallback) | **Verified** |
| PG-as-queue (event_outbox, ai_jobs, receipts, webhook/bot/stream outboxes, blob GC) | **Durable**, claim/lease/fencing/SKIP LOCKED/backoff | **Verified** relay loops in `background.rs` |

## Findings (7; none ship-blocking)
1. **Low** — F1 fences verified on all 3 lockless UPDATEs + edit; system-edit paths fence in code under row lock (correct, test-proven). Recommend defense-in-depth SQL `AND recalled_at IS NULL` there too.
2. **Low** — AI worker can pay for an embedding recall discards (cost race, no corruption); misleading log message.
3. **Low** — Redis seq keys regress across a *lossy* Redis restart → client `SeqGate` false-dedup (≤256 events); stock compose is AOF-safe, requirement undocumented.
4. **Medium (scale-dependent)** — Migration 0238 holds SHARE lock on `messages` + ACCESS EXCLUSIVE on `event_outbox` in one tx (index DROP+CREATE, CHECK swap, shadow backfill join): write-blocking window on the hot table during migration-first upgrades; measure first.
5–7. **Info** — relay suppression of superseded `Recalled` events correct; GDPR erasure-over-recall benign; F3 gate + new authz lint verified on both WS and REST paths.

## Verified hot/atomic paths
Recall tx (row lock + one-shot WHERE fence + redacted snapshot + blob GC + audit + outbox at `MAX(aggregate_version)+1` — exactly-one-winner under concurrency), relay claim predicates all index-covered, `search_tsv` STORED-column reasoning for F1, `changes_since`/`idx_messages_room_mutated` expression alignment, Redis INCR rate/seq paths.

## Migration safety
0238 is additive + strict-superset CHECK (old binaries compatible); **rollback not recommended once recalls exist** (would re-open placeholder as editable content); roll-forward per convention. Included 5 validation queries, including the F1 canary: `SELECT count(*) FROM messages WHERE recalled_at IS NOT NULL AND (searchable_text <> '' OR embedding IS NOT NULL)` — must be 0.

**Unknowns**: table volumes (determine Finding 4 window), throughput vs 16-conn pool, retention default values, RPO/RTO (no backup drill in repo).

Evidence honesty: I ran `cargo check --workspace --all-targets` (exit 0) and read all cited code/migrations; PG-gated tests and load measurements were **not** run — labeled accordingly.
