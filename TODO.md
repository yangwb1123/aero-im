# TODO — Autonomous Factory backlog

> Working backlog for the 24×7 factory loop. Strategy/why lives in `ROADMAP.md`
> (第五版); architecture in `AGENTS.md` §1 + `docs/specs/`; history in `git log`.
> Each item: status · scope-class (auto-safe | STOP-needs-decision) · anchor.

## P0 — correctness / deployability (priority #1)

- [ ] **BLOCKER · STOP — migration chain breaks at 0109 on a fresh DB.**
  `0109_subscription_tiers.sql:12` does a bare `ALTER TABLE creator_subscriptions
  ADD COLUMN tier_id ...`, but `0051_creator_subscriptions.sql:26` already created
  `creator_subscriptions.tier_id uuid NOT NULL` and nothing drops/renames it →
  `ERROR: column "tier_id" already exists`. **A brand-new deployment cannot
  migrate.** Hidden because the dev DB is frozen at migration 32 (pre-0109), all
  db-tests are `#[ignore]` (never run a fresh chain in CI), and migrations are only
  `cargo build`-embedded, never execution-validated. Two tier systems coexist
  (`creator_tiers` from 0051 vs `subscription_tiers` from 0109). **Needs a product
  decision** → see DECISIONS.md ADR-002 (replace-and-migrate vs parallel+rename).
  Owner: human. _Also add a CI job that replays `migrations/*.sql` on a scratch DB._
- [ ] **STOP — gift double-charge (money path).** `aero-storage/src/live.rs:~125`
  mints `Ulid::new()` + bare INSERT with no idempotency key; a retried gift RPC
  double-charges. Needs schema (idempotency-key column / unique constraint) +
  public `Idempotency-Key` header → interface+schema change → confirm before build.
- [ ] **auto-safe — bus-listener reconnect black hole.** `ws.rs:~969`
  `run_bus_listener` / `run_live_bus_listener` loop `while let Some(x) =
  stream.next()`; on a NATS reconnect the subscription stream ends and the loop
  exits permanently → that process stops fan-out (silent total delivery outage).
  Wrap in an outer resubscribe/backoff loop. No interface/schema change. **Next round.**
- [x] ~~GDPR erasure left message `embedding` populated (re-identifiable).~~
  Fixed 984bffc — `embedding = NULL` in `delete_participant` + live-verified db-test.

## P1 — depth / correctness edges

- [ ] **STOP — legal-hold vs right-to-erasure.** `delete_participant` erases all
  sender messages unconditionally; messages under an active legal hold should be
  exempt (or the conflict resolved explicitly). Policy decision required.
- [ ] End-to-end distributed tracing + SLO surfacing (ROADMAP 第五版 P0-二).
- [ ] Agentic AI + knowledge-base direction (ROADMAP 第五版 P1).
- [ ] Data-lifecycle / GDPR-correctness sweep audit (ROADMAP 第五版 P1).

## P2

- [ ] Auth-abuse depth / poison-message DLQ for bus listeners (ROADMAP 第五版 P2).

## Tech debt (see also DECISIONS.md)

- **High:** migrations never execution-validated (root cause of the 0109 blocker) —
  add a fresh-DB migration smoke to CI.
- **Medium:** dev DB frozen at migration 32 — drift from the 125-migration HEAD
  masks any fresh-deploy schema bug.
- **Low:** db-tests are `#[ignore]`-only; no automated live-PG lane.
