# DECISIONS — Architecture Decision Records

> Append-only. One ADR per significant decision. Status: PROPOSED · ACCEPTED ·
> PENDING (needs human) · SUPERSEDED.

## ADR-001 — GDPR right-to-erasure must null the message embedding

- **Date:** 2026-06-13
- **Status:** ACCEPTED (implemented, commit 984bffc)
- **Decision:** `ParticipantRepo::delete_participant` adds `embedding = NULL` to the
  message-anonymisation UPDATE, alongside the existing `blocks` / `searchable_text`
  clearing.
- **Reason:** A pgvector embedding is a semantic fingerprint of the original text;
  a nearest-neighbour search over a retained embedding reconstructs what was
  supposedly erased, so clearing only the text left erasure incomplete (GDPR Art. 17
  re-identification). All sibling content-clearing paths already null it
  (`message.rs:101/154/274`, `workspace.rs:826`); the erasure path was the outlier.
- **Impact:** Erased messages lose semantic searchability (intended). Added a
  PG-gated regression test (`participant::db_tests::erasure_nulls_message_embedding`).
- **Alternatives:** Leave embedding (rejected — re-identifiable); delete the row
  entirely (rejected — breaks thread/FK integrity, hence the placeholder approach).

## ADR-002 — Reconcile the two tier systems blocking migration 0109 (PENDING)

- **Date:** 2026-06-13
- **Status:** PENDING — needs a product/human decision (blocks fresh deploys).
- **Context:** `0051_creator_subscriptions.sql` created `creator_tiers` +
  `creator_subscriptions.tier_id uuid NOT NULL`. Later, `0109_subscription_tiers.sql`
  introduced a *second* tier table `subscription_tiers` and tried
  `ALTER TABLE creator_subscriptions ADD COLUMN tier_id UUID REFERENCES
  subscription_tiers(id)` — colliding with the existing `tier_id`. A fresh DB fails
  at 0109. The dev DB (frozen at migration 32) and `#[ignore]`-only db-tests hid it.
- **Options:**
  - **A. Parallel + rename (smallest, reversible):** rename 0109's new column to
    `subscription_tier_id` (FK to `subscription_tiers`, nullable). Both tier systems
    coexist. Lowest risk; leaves two overlapping concepts.
  - **B. Replace-and-migrate:** treat `subscription_tiers` as the canonical system,
    backfill from `creator_tiers`, repoint `creator_subscriptions.tier_id` (add FK,
    make nullable), deprecate `creator_tiers`. Cleanest end state; data migration risk.
  - **C. Make 0109 idempotent only:** `ADD COLUMN IF NOT EXISTS` — rejected, leaves
    `tier_id` without the intended FK/nullability (semantically wrong).
- **Recommendation:** **A** now (unblock fresh deploys with minimal risk) +
  schedule **B** as a follow-up once product confirms the canonical tier model.
  Either way: add a CI job that replays `migrations/*.sql` against a scratch DB so
  the chain is execution-validated, not just compile-embedded.

## ADR-003 — Bus listeners resubscribe across NATS reconnects

- **Date:** 2026-06-13
- **Status:** ACCEPTED (implemented, commit e5f1fb1)
- **Decision:** `run_bus_listener` / `run_live_bus_listener` wrap their
  subscribe + consume loop in an outer `loop` that re-subscribes (1s backoff) when
  the subscription stream ends or a subscribe fails, instead of returning.
- **Reason:** The stream ends on a NATS reconnect/drop; the old code returned
  `Ok(())`, the boot-time task terminated, and nothing re-spawned it — the process
  silently stopped all room/stream fan-out with no error. A resubscribe loop is the
  standard durable-consumer pattern.
- **Impact:** The room listener's durable consumer (`aero-server`) resumes from its
  committed cursor → at-least-once preserved across reconnects. The live listener is
  ephemeral by design (broadcast; a few dropped danmaku across a reconnect are
  immaterial) but its loop now survives. Backoff is non-zero so a hard-down NATS
  can't spin a tight loop.
- **Alternatives:** Crash the process on stream-end and rely on an orchestrator to
  restart (rejected — drops every other in-process listener/session); per-task
  supervisor that re-spawns (rejected — heavier, same effect as the inline loop).
- **Test gap:** no regression test — `run_bus_listener` needs a full `AppState` and
  aero-server has no test-AppState harness (see TODO.md tech debt).
