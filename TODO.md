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

- [x] ~~**legal-hold vs right-to-erasure.**~~ Fixed 0a4be67 — erasure now exempts
  messages under an active legal hold (GDPR Art. 17(3)(e)), mirroring the retention
  sweep. Follow-up: complete erasure on hold-release (needs a deferred-erasure queue).
- [ ] End-to-end distributed tracing + SLO surfacing (ROADMAP 第五版 P0-二). *Large.*
- [ ] Agentic AI + knowledge-base direction (ROADMAP 第五版 P1). *Large.*
- [x] ~~Data-lifecycle / GDPR-correctness sweep audit (ROADMAP 第五版 P1).~~ Direction
  CLOSED + went beyond the scan via a completeness audit of `delete_participant`:
  message embedding (984bffc), legal-hold exemption (0a4be67), deferred-erasure
  sweep (219aca5), **identity PII** — name/avatar/email/password/phone/profile/SSO
  (50eaa2c), and **authored content** — drafts/OOO/scheduled (f89ad4d). Erasure is
  now comprehensive (identity + all authored content), hold-aware, and eventually
  consistent. Audited-clean: channel-points/predictions spend is atomic (no double-spend).

## P2

- [x] ~~poison-message loop for bus listeners.~~ Fixed 3613b24 — undecodable payloads
  are ack-dropped (not nacked forever) + `aero_bus_poison_dropped_total` metric.
- [ ] Auth-abuse depth (ROADMAP 第五版 P2). *Larger; rate-limit + lockout deepening.*

## Flagged design questions (need product intent, not autonomous fixes)

- **Blocking enforcement scope.** `is_blocked` is checked ONLY at 1:1-DM-open
  (`dm.rs:93`). It is NOT enforced in `send_message`, `start_call`, group-DM
  inclusion, mention/notify fan-out, or reactions — so a block does not retroactively
  disable a pre-existing DM room (a cached `room_id` still routes calls/messages),
  nor hide a blocked user's content in shared rooms. If the intended semantics are
  "block only prevents opening a new DM," this is correct as-is; if it should be a
  full mute/cloak (no calls, no messages delivered, content hidden), several surfaces
  need an `is_blocked` gate. **Needs product decision on intended blocking scope.**
  (Audited-clean this pass: AI/RAG retrieval is membership-guarded across all ask
  handlers + authz_lint; channel-points/predictions spend is atomic.)

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
