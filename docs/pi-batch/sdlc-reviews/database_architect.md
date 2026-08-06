Review complete. Written to `docs/pi-batch/sdlc-reviews/database_architect.md`.

## Summary

**Prompt rules**: repo has no `prompts/README.md` (confirmed — same as the perf review noted); applied `/home/u1/ai-batch-runner/prompts/README.md`.

### Store inventory (verified against working tree + boot wiring)
- **PostgreSQL** — durable source of truth (messages/outbox/audit/message_edits); wired at boot with retry + `migrate()`; READ COMMITTED, 10s statement timeout.
- **NATS JetStream** — durable for `im.room.*` (File stream, 7d, per-instance durable consumers), ephemeral for `live.stream.*`; wired at boot.
- **Redis** — hot/volatile (presence, viewers, call roster, **seq counters**); stock compose runs `--appendonly yes`; fail-open reads.
- **Blob** — LocalFs default / S3/vault fail-loud; **HLS** local FS; **Hub/SFU** in-process, not stores.
- The recall window is pure PG-side policy: **zero new backend, zero new migration this batch (0238 shipped earlier), env wired into the stock binary** (`orig.rs:316` ← sole prod call site `boot/services.rs:75`).

### Findings (sorted)
- **F1 Medium** — 0238 drops the 0125 index; during mixed-version deploys (0176 supports them) old binaries' `changes_since` loses its index (seq scan) and can't see recalls (stale content). Fix: expand–contract (keep both indexes) or document atomic cutover.
- **F2 Low** — `messages_partitioned` shadow reconcile covers only recall columns; other columns stay stale until cutover (pre-existing 0148 convention, dormant).
- **F3 Low** — boot-time blocking DDL on `messages` (no `CONCURRENTLY` anywhere in 238 migrations).
- **F4 Low** — Redis seq reset would mint duplicate seqs (contract tolerates gaps, not resets); compose-safe, ops note otherwise.
- **F5 Info** — 10s statement timeout includes lock-wait; recall tx aborts cleanly under contention, retryable.
- **Verified non-findings**: no clock mixing (all 25 non-canonical `INSERT INTO messages` sites are test fixtures; prod path binds app-clock `created_at`); `recalled_by` FK safe under GDPR tombstone; no new lock edge/deadlock (lock order identical to edit/delete, window check is zero SQL); bus poison ack-drops (no redelivery loop); outbox version-gating serializes recalled events.

### Transaction analysis
Recall tx = ~12–16 statements, single tx, window evaluated in-process on the `FOR UPDATE` snapshot (atomic by construction — SQL predicate correctly rejected). Lock order: workspace → room FOR SHARE → room_members → messages → blobs (sorted) — no new edges. Preflight is UX-only, re-checked under locks.

### Migration sequence
0238 is additive + idempotent; roll-forward = build → migrate → roll binaries; rollback of the **schema** is lossless, but rollback of the **feature** (recalled content) is impossible by design — only the redacted snapshot survives. 6 validation queries included.

### Unknowns
No backup/restore/PITR tooling in repo (RPO/RTO unknown — flagged for a feature whose terminal state is non-recoverable); no table-size evidence (index-build and lock-wait costs unquantified); event_outbox sweep retention target undocumented.

**VERDICT: PASS** — persistence-correct, with F1/F3 as deployment-window hardening and backup coverage as the missing operational control.
