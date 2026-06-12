# TODO — Autonomous Factory backlog

> Working backlog for the 24×7 factory loop. Strategy/why lives in `ROADMAP.md`
> (第五版); architecture in `AGENTS.md` §1 + `docs/specs/`; history in `git log`.
> Each item: status · scope-class (auto-safe | STOP-needs-decision) · anchor.

## P0 — correctness / deployability (priority #1)

- [x] ~~**BLOCKER — migration chain breaks at 0109 on a fresh DB.**~~ Fixed 40f8cab:
  the colliding `ADD COLUMN tier_id` was dead (no code referenced it), so it was
  dropped outright (DECISIONS.md ADR-002). Full chain now replays clean (125/125),
  guarded by `scripts/migrate_chain_smoke.sh` / `make migrate-smoke` (4495215).
- [x] ~~**db-tests never executed against a full schema.**~~ Swept all crates against
  a fully-migrated DB: aero-storage had 3 broken fixtures (`body`/`email`/`password_hash`
  columns that don't exist) — fixed 57c2d45 (195/0); aero-im-core (11) + aero-server
  (5) were already correct. db-tests now genuinely pass workspace-wide on a fresh DB.
- [x] ~~**gift double-charge (money path).**~~ Fixed c1abddd — optional idempotency
  key (REST `Idempotency-Key` header / WS `nonce`) + partial unique index (mig 0126);
  `insert_gift` ON CONFLICT DO NOTHING, `send_gift` skips broadcast/goals/hype-train
  on a dedup hit. Additive/backward-compatible. Live-verified db-test. ADR-004.
- [x] ~~**bus-listener reconnect black hole.** `run_bus_listener` /
  `run_live_bus_listener` returned permanently when the NATS stream ended on a
  reconnect → silent total fan-out outage.~~ Fixed e5f1fb1 — outer resubscribe loop
  (1s backoff); durable cursor preserves at-least-once. See DECISIONS.md ADR-003.
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

- **~~High~~ → mitigated:** migrations never execution-validated (root cause of the
  0109 blocker). `scripts/migrate_chain_smoke.sh` now replays the chain on a fresh
  DB; remaining: wire it into an actual CI workflow (no CI runner in this sandbox).
- **Medium:** dev DB frozen at migration 32 — drift from the 125-migration HEAD
  masks any fresh-deploy schema bug. Mitigated for new bugs by the smoke script, but
  the dev DB itself should be re-provisioned from a clean chain.
- **Low → mitigated:** db-tests were `#[ignore]`-only and never run; now verified to
  pass against a fully-migrated DB workspace-wide. Still no *automated* live-PG CI
  lane (would run `--ignored` against a postgres service).
- **Medium:** no test-AppState harness in aero-server — its 286 lib tests are all
  pure-function, so background tasks (bus listeners, sweeps) and full route flows
  have no unit coverage. Blocks e.g. a resubscribe regression test for ADR-003.
