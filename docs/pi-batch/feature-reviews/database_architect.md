Review complete and written to `docs/pi-batch/feature-reviews/database_architect-r2.md` (round-1 file preserved). Note: `prompts/README.md` doesn't exist in the repo — I applied the shared rules from `/home/u1/ai-batch-runner/prompts/README.md`. All evidence was re-derived from current source at the 0238 revision.

## Summary

**1. Store inventory (hot vs durable, stock wiring) — all Verified**

- **PG** = durable system of record (messages/children, outbox, idempotency ledgers, worker queues, delivery cursors); optional read replica routed only via explicit `QueryConsistency::Eventual`.
- **PG `event_outbox`** = durable producer outbox (incl. new `recalled` kind), relay spawned unconditionally in `bin/boot/background.rs` (250 ms poll), `SKIP LOCKED` + 30 s lease + attempts fencing.
- **Redis** = hot soft state (presence/viewer 256-shard zsets, call-roster Lua with generation fencing) + hard counters (`aero:seq:*` INCR, fail-open on loss) + ws-rate windows; all wired in `bin/boot/repos.rs`.
- **NATS JetStream** = durable `im.room.*` (7 d) + ephemeral live consumer; stable `event_id` as `Nats-Msg-Id` dedup.
- **Blobs**: LocalFs default (per-node disk, not cluster-durable) → S3/AeroVault env-gated, **fail-loud** on incomplete config; compose MinIO is *not* used unless `AERO_BLOB_BACKEND=s3`.
- **`messages_partitioned`** = cold staging shadow (cutover stays in the runbook). Every durable path is wired by the stock binary.

**2. Findings (severity-sorted)**

| # | Sev | Finding |
|---|---|---|
| F1 | **HIGH** (scale-conditional) | Migrations run on the same pool whose `after_connect` sets `statement_timeout=10s` (`db.rs`) — 0238's `CREATE INDEX` on `messages` + the shadow reconcile join can abort the chain mid-migration on production-sized tables; `CONCURRENTLY` unusable under sqlx tx. Green gates only prove empty-DB replay. |
| F2 | LOW/MED | `/changes` is single-page, clamped to 200, web client advances its cursor *before* the reply and never loops — >200 mutations per reconnect window (incl. recalls) are lost forever, leaving stale pre-recall content. |
| F3 | LOW | No gauge for `event_outbox` pending depth (only NATS consumer lag) — NATS-down backlog is invisible. |
| F4 | LOW | `recalled_by` survives GDPR erasure (`participant.rs` doesn't NULL it); needs wiring or an explicit documented stance. |
| F5/F6 | INFO | NATS 7 d < outbox 30 d (bots down >7 d lose events — documented boundary); recalled rows linger in partial FTS/trgm GIN (negligible). |
| F7 | **RESOLVED** | Round-1 F1 (stale backfill projection) verified fixed — current 0238 reissues `backfill_messages_partition` with `recalled_at/by` in both column lists + shadow reconcile + expression-index reissue matching `changes_since`. |

**3. Hot/atomic paths analyzed with query/index evidence:** insert tx (fence → locks → outbox `MAX+1` under row lock → side effects → idempotency claim, one commit), recall tx (role re-check under `FOR UPDATE`, final `WHERE recalled_at IS NULL` fence, history/audit/GC/outbox in-tx), outbox claim (all four partial indexes match predicates), `changes_since` ↔ reissued expression index, Redis sharding/generation fencing, `SKIP LOCKED` worker queues.

**4. Migration plan:** 0238 additive (2 nullable cols + CHECK reissue + index/backfill reissue); old binaries poison-drop `recalled` events → upgrade-all-nodes-first; validation SQL (stuck-row, shadow-parity, edit-snapshot queries); roll-forward idempotent; rollback = binary revert first, delete pending `recalled` outbox rows, drop cols, restore 0125 index.

**5. Unknowns:** no benchmarks/load tests anywhere in the tree — pool sizing, statement-timeout, page size, shard count all unvalidated at scale; no backup/restore automation beyond runbook guidance (compose volumes only); Redis seq keys unbounded by design; message history unbounded (audit stance). Required measurements listed in §5 of the file.

No Critical findings. The two items to fix before any real-scale rollout are F1 (migration timeout) and F2 (`/changes` continuation).
