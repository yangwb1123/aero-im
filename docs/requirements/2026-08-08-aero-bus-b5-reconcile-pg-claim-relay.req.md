# Requirements Spec — Reconcile the aero-bus B5 transport seams with the landed PG-claim relay: SUPERSEDE decision, H1 closure, lib.rs BR6, b5-pin honesty

- **Module**: `crates/aero-bus/src` (+ the seven in-repo aero-bus B5 requirement/design docs that mandate the unlanded NATS transport; harness `scripts/b5-pin.sh` / `scripts/test-b5-pin-guard.sh` are verification surfaces, not change surfaces)
- **Direction**: "Reconcile aero-bus B5 transport seams (audit.ws.*/AUDIT_EVENTS, audit.priority.*/AUDIT_PRIORITY) with the landed PG-claim relay: supersede-or-wire decision, close handoff H1, fix lib.rs/BR6 and b5-pin honesty" — three in-repo aero-bus B5 specs (…b5-1-audit-outbox-status-machine, …b5-3-priority-delivery-seam, …live-nats-acceptance-suite + matching design docs) mandate a NATS transport (claim → publish `audit.priority.<id>` → `AUDIT_PRIORITY` WorkQueue stream → connector durable subscribe → HTTP sink) that has zero code and zero consumers; the landed architecture achieves moderation priority via claim ordering and T-11 via connector 403→dead, so G6 is satisfiable without the bus seam; leaving the specs live invites a redundant NATS hop that must not bypass the connector's claim/provision gates; open reconciliation debt: 0239's handoff H1 comment and the stale `lib.rs` 3-stream doc.
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-src-3f54dec6.json` (direction #1; value 8 / risk-reduction 8 / effort 3 / confidence 9)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-08 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-bus/src/jetstream.rs` — `bootstrap()` declares **4** streams, no audit stream | ✅ **Verified**. `bootstrap()` declares `IM_MESSAGES` (`im.room.*`, Limits, 7d, duplicate_window=max_age), `IM_EVENTS` (`im.events.>`, Limits, 30d), `AI_QUEUE` (`ai.queue.*`, WorkQueue, 1d), `LIVE_EVENTS` (`live.stream.*`, Limits, 6h, 200k). **No `AUDIT_EVENTS`, no `AUDIT_PRIORITY`** — the audit seam has zero bootstrap code. |
| E2 | `subscribe()` inline 4-prefix map, fail-closed unknown prefix | ✅ **Verified**. Inline `starts_with` chain: `im.room.`→`IM_MESSAGES`, `im.events.`→`IM_EVENTS`, `ai.queue.`→`AI_QUEUE`, `live.stream.`→`LIVE_EVENTS`, else `Err(BusError::Nats(format!("unknown subject prefix: {subject}")))`. No `stream_for_subject` extraction exists (it is design-doc-only). |
| E3 | `poison_safe_pull_config` + `validate_publish_subject` | ✅ **Verified**. `poison_safe_pull_config`: `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` for durable non-`aero-server-*`, `New` for ephemeral/process-local — generic, no audit special-casing. `validate_publish_subject` rejects empty/wildcard/whitespace, accepts concrete subjects. No audit subject family anywhere in the file (fixed-string `rg 'audit\.(priority|ws)\.' crates/aero-bus` = **0 hits**). |
| E4 | `crates/aero-bus/src/lib.rs:9-13` — stale 3-stream doc (BR6) | ✅ **Verified**. Doc lists only `IM_MESSAGES` / `IM_EVENTS` / `AI_QUEUE`; `bootstrap()` declares **4** — `LIVE_EVENTS` missing. The "Implementation is filled in by the storage/bus agent" tail line is also stale scaffolding. |
| E5 | `crates/aero-audit-connector/Cargo.toml` — no aero-bus dependency | ✅ **Verified**. `grep aero-bus crates/aero-audit-connector/Cargo.toml` = 0 hits; dependencies are aero-common + aero-auth + network/DB crates only. The connector has **zero** bus code paths. |
| E6 | `crates/aero-audit-connector/src/pg.rs:117` — claim `ORDER BY priority DESC` | ✅ **Verified**. `claim_due` CTE: `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id` + `FOR UPDATE SKIP LOCKED` + `LIMIT $1`; status filter `IN (0, 1)`, lease filter, `attempts+1` post-increment, `claim_token = gen_random_uuid()` per claim. Status constants `STATUS_ENQUEUED..STATUS_DEAD` (pg.rs:27-30) single-source from `aero_common::model::audit::OutboxStatus` (leaf, `crates/aero-common/src/model/audit.rs:20`, explicit discriminants 0..3). |
| E7 | `crates/aero-audit-connector/src/relay.rs` — poll loop claims PG and POSTs directly | ✅ **Verified**. `run()` (interval + cancel drain) → `dispatch_batch()` (`reconcile` → `claim_due` → per-claim `deliver_claim`) → `client.deliver(&claim)` HTTP POST; success → fenced `settle`; **403 → `mark_dead` immediately** (T-11 fail-closed, no requeue); permanent → requeue on attempt 1, dead from `PERMANENT_DEAD_AT=2`; transient → fenced `requeue` with `audit_backoff`. **No NATS anywhere in the delivery path.** |
| E8 | `crates/aero-audit-connector/src/config.rs` — `from_env` fail-loud | ✅ **Verified**. Presence-gated on `AERO_AUDIT_TOKEN_ENDPOINT`; any other `AERO_AUDIT_*` var set while it is absent is a boot error (never silent disable); lease bails cloned from v1. |
| E9 | `crates/aero-server/src/bin/main.rs:245-261` — relay wiring | ✅ **Verified**. `RelayConfig::from_env()` → `Ok(Some)` ⇒ `PgOutboxRepo::new(persistence.pg)` + `AuditClient::new` + `AuditRelay::new(repo, client, cfg)` + `tracker.spawn(relay.spawn(ai_shutdown))`; `Ok(None)` ⇒ disabled; `Err` ⇒ boot error (fail-loud). Boots pre-0239 degrade to logged claim errors, not a crash. |
| E10 | `migrations/0239_audit_governance_outbox.sql` — status 0/1/2/3, DEFAULT 10, H1 comment | ✅ **Verified**. `status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`; `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)` with pin comment "DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG (governance.rs:33)"; `class`/`delivery_mode` CHECKs; lease/attempts/backoff; due index; token-keyed trigger (`message.moderated` → class 'admin', priority 100, envelope `admin.content.flag`); **handoff H1 comment**: "B5-3's aero-bus design doc must reconcile its 'DEFAULT 0' to 10 at landing (handoff H1 in the 0239 design doc)." |
| E11 | `migrations/0240_audit_governance_due_prio_idx.sql` | ✅ **Verified**. `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` — matches pg.rs claim ORDER BY exactly (LIMIT pushdown); 0239's FIFO index deliberately kept (rolling deploy), legacy drop is a later cleanup (design decision D5). Also `0241_governance_reconcile.sql` (disabled-window reconciler) is landed. |
| E12 | `scripts/b5-pin.sh:23-46` — 37 slots: 15 executed + 22 `[PROPOSED]` | ✅ **Verified**. `B5_CONTRACT_TEST_LIST` = 15 executed (`rolling_upgrade_fences…` … `audit-provision-check`) + 22 `contract-test-NN[PROPOSED]`. **None of the 15 executed slots is backed by the bus seam** — verdict sources are: migration-regression filters, `audit_governance::` + `moderation_finalize_outbox_parity` db_test filters, `a3-relay-drill` / `t11-fail-closed` / `moderation-priority-drill` / `relay-mock-probe` / `audit-provision-check` / `notification-fanout` legs — all landed code (see E13/E14). `assert_b5_contract_pin` enforces exactly 37, no dupes, non-vacuous, verdict-line evidence per executed slot. |
| E13 | `scripts/test-integration.sh:436,441-461` — t11 + moderation-priority drill PASS | ✅ **Verified**. `b5_check "t11-fail-closed" "PASS"` at :436 (T-11 drill: relay absent ⇒ rows stay pending, never falsely delivered/dead; connector 403→dead leg C); moderation-priority drill block :441-461 (`aero-cli audit-provision-check --priority`, 500 backlog + 1 moderation, verdict `priority: landed`, `b5_check "moderation-priority-drill" "PASS"`), gated on the 0239 file existing. Both verdict sources are **claim-ordering/connector code**, not the bus seam. |
| E14 | Other executed-slot verdict sources (supplementary) | ✅ **Verified**. `aero-audit-relay-drill` bin at :335 (→ `B5-CHECK a3-relay-drill: PASS` :338); `audit-provision-check` legs at ~:296; `audit_governance::` / `moderation_finalize_outbox_parity` cargo-test filters :309/:313 (aero-storage `audit_governance.rs` db_tests, incl. `ddl_contract_defaults_and_checks` at :425); `notification-fanout` via `scripts/test-notification-fanout.sh`; `relay-mock-probe` at :533-573. Drill bins exist: `crates/aero-audit-connector/src/bin/{aero-audit-t11-drill,aero-audit-priority-drill,aero-audit-relay-drill}.rs`. |
| E15 | The four requirement + three design docs mandate the unlanded seam | ✅ **Verified (mandate confirmed, code absent)**. `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` (BR1–BR6: `audit.ws.*`/`AUDIT_EVENTS`, `AUDIT_SUBJECT_PREFIX`, `stream_for_subject` extraction, exactly-N expansion), `2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (BR1–BR6: `audit.priority.*`/`AUDIT_PRIORITY` WorkQueue, `AUDIT_PRIORITY_SUBJECT_PREFIX`), `2026-08-07-aero-bus-live-nats-acceptance-suite.req.md` (BR1–BR5: 7 live-NATS tests + 2 harness legs + 37→39 manifest re-baseline), `2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` (the "land the seam" spec), and matching `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` / `…b5-3-priority-delivery-seam.design.md` / `…live-nats-acceptance-suite.design.md`. `rg 'bus-priority-seam|bus-audit-events' scripts/` = 0 hits — the harness legs exist **only in the design doc**. |
| E16 | Handoff H1 state | ✅ **Verified (normatively closed, annotation residue remains)**. 0239 design doc H1 row (`docs/design/2026-08-07-aero-ai-b5-1-migration-0239-audit-governance-outbox.design.md:280`): "Reconcile the aero-bus doc at B5-3 landing: DEFAULT 0 → 10." The b5-3 aero-bus design doc now pins **`priority SMALLINT NOT NULL DEFAULT 10`** at :145 and :193 ("H1 reconcile: the draft's DEFAULT 0 landed as 10 = `GOVERNANCE_PRIORITY_BACKLOG`"). `grep 'DEFAULT 0' docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` = **2 hits, both the reconciliation annotations themselves** (:144/:193) — no *normative* "DEFAULT 0" remains. The 08-08 verified-landing design doc's closure check (`:158`: "grep … must be empty") is therefore **imprecise**: closure criterion is "no normative DEFAULT 0", not "zero string occurrences" (see R-3). |
| E17 | `crates/aero-ai/src/governance.rs` — priority constants | ✅ **Verified**. `GOVERNANCE_PRIORITY_MODERATION: i16 = 100` (:31), `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` (:33, doc "also the 0239 column default"); DESC = higher-first explicitly documented as the INVERSE of ai_jobs ASC. |
| E18 | `scripts/test-b5-pin-guard.sh` guard self-test | ✅ **Verified**. Pure bash; hardcoded literals "B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS" (positive + SKIP_DB_CREATE cases), count-36-fails, duplicate/malformed indices `[36]`, vacuous 37-`[PROPOSED]` case. Any manifest re-baseline must update these in lockstep — this spec keeps 37, so the guard is untouched. |
| E19 | `docs/campaigns/implementation-gate.md:78` — G6 row | ✅ **Verified**. `| G6（B5） | B5-1..4 | 37/37、T-11、moderation 优先级 |`. The gate names no bus legs today; each component is satisfiable without the seam (E12/E13). |
| E20 | (supplementary) `scripts/test-integration.sh:17,489` — `AERO__NATS__URL` knob + export; Step 4 blanket `--ignored` | ✅ **Verified**. :17 comment; :489 `export AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}"`; Step 4 runs `cargo test --workspace --lib --locked -- --ignored --test-threads=1` — the single existing live-NATS test (`live_nats_deduplicates_same_message_id`, IM_EVENTS dedup) already executes in-harness when NATS is reachable. No audit-namespace live test exists anywhere (`rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0). |

**Verdict**: every cited symbol verified against the tree; zero fabrication. The direction's core claim — the seam has zero code, zero consumers, and G6 is satisfiable without it — is confirmed: the only aero-bus-side truth gap is the stale `lib.rs` doc (E4) and the unclosed *records* of the supersession (E15/E16).

## 2. Verified current state (what this direction reconciles)

```
LANDED audit delivery pipeline (aero-audit-connector, no bus hop):
  audit_events (L0, partitioned 0146)
    └─ 0239 trigger aero_enqueue_governance_audit (token-keyed: ONLY 'message.moderated',
        class 'admin', priority 100, envelope 'admin.content.flag') + 0241 reconciler
         └─ audit_governance_outbox (0239: status 0/1/2/3 CHECK, priority DEFAULT 10
             CHECK (>0), lease/attempts/backoff, due index WHERE status IN (0,1);
             0240: due+prio index (priority DESC, …))
              └─ PgOutboxRepo::claim_due  ORDER BY priority DESC  (pg.rs:117)   ← moderation priority
                   └─ AuditRelay::run → dispatch_batch → client.deliver (HTTP POST)
                        └─ settle (2) / requeue (transient + permanent-1) / mark_dead
                           (403 = T-11 fail-closed, permanent ≥2)                ← T-11
                           └─ HTTP sink (Idempotency-Key = event_id)

UNLANDED aero-bus seam (docs only — zero code, zero consumers, zero harness legs):
  audit.priority.* → AUDIT_PRIORITY (WorkQueue) → connector durable subscribe → sink   (b5-3 spec BR1-6)
  audit.ws.*       → AUDIT_EVENTS (Limits, 7d)    → exactly-N expansion + per-subject seq (b5-1 spec BR1-6)
  live-NATS suite (7 tests) + bus-priority-seam / bus-audit-events harness legs + 37→39  (live-nats spec BR1-5)

aero-bus (this module):
  bootstrap() ── IM_MESSAGES · IM_EVENTS · AI_QUEUE · LIVE_EVENTS        (E1: 4 streams, no audit)
  subscribe() ── 4-prefix inline map, fail-closed on unknown             (E2)
  lib.rs:9-13 ── documents 3 streams, missing LIVE_EVENTS               (E4: stale — BR6 debt)
  grep 'audit\.(priority|ws)\.' crates/aero-bus ── 0 hits               (E3: no audit publish path)
```

**The supersede-or-wire decision, with evidence**:

| Option | Consequence | Verdict |
|---|---|---|
| **Wire** the seam (implement AUDIT_EVENTS/AUDIT_PRIORITY per the four req + three design docs) | Adds a second delivery path (PG → NATS → connector durable subscribe → HTTP). The b5-3 spec's own A5/T-11 constraint demands this path must not bypass the connector's claim/provision gates — so the publisher must be a *third* component (relay + bus publisher + connector), each hop re-proving lease fencing, dedup (Nats-Msg-Id), seq stamps, and WorkQueue ack semantics for **zero added delivery guarantee**: PG `audit_governance_outbox` is already the durable queue, `claim_due` already orders priority, `settle`/`requeue`/`mark_dead` already implement the status machine, and 403→dead already provides T-11. The live-NATS suite's 7 tests + 2 harness legs + 37→39 re-baseline would be built to test a transport that transports nothing new. | **Rejected** — redundant hop, second delivery path, new surface for gate-bypass bugs. |
| **Supersede** (record the decision; keep the landed PG-claim relay as the single delivery path) | G6 stays green today: 37/37 (E12, all 15 executed slots backed by landed code), T-11 (E13 :436 + connector 403→dead, E7), moderation 优先级 (E13 :441-461 + pg.rs:117 priority DESC + 0240 index). The A5 fail-closed invariant holds **by construction**: `rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0 — there is no bus subject the sink could consume without the connector's gates because there is no audit bus subject at all. Cost: seven docs must carry an explicit superseded pointer so no future implementer rebuilds the seam. | **Adopted** — matches landed architecture; closes the second-delivery-path hazard. |

## 3. Scope

**In scope (this direction)**:
- The **supersede-or-wire decision**, recorded normatively (BR1): the aero-bus B5 NATS transport seams are **superseded** by the landed PG-claim relay; no `AUDIT_EVENTS`/`AUDIT_PRIORITY` stream, no `audit.ws.*`/`audit.priority.*` subject namespace, no `stream_for_subject` extraction, no `AUDIT_SUBJECT_PREFIX`/`AUDIT_PRIORITY_SUBJECT_PREFIX` constants, no exactly-N expansion contract will be built.
- **Superseded pointers** in the seven aero-bus B5 requirement/design docs (BR2) — each doc carries an explicit "superseded by `PgOutboxRepo::claim_due` priority DESC" pointer so the decision is discoverable at the mandate site.
- **Handoff H1 closure record** (BR3): the 0239 migration comment's handoff is discharged — verified against the b5-3 design doc's normative DEFAULT 10 pin, the landed 0239 DDL, `GOVERNANCE_PRIORITY_BACKLOG` (governance.rs:33), the behavioral db_test `ddl_contract_defaults_and_checks`, and the 0240 index; the two remaining "DEFAULT 0" strings are the reconciliation annotations themselves and are not normative.
- **lib.rs BR6 doc fix** (BR4): `crates/aero-bus/src/lib.rs` stream list updated to the 4 streams `bootstrap()` actually declares — the **only** production-file change in this module.
- **b5-pin honesty** (BR5): verified that no executed slot is backed by the unlanded seam; slot count **stays 37** (15 executed + 22 `[PROPOSED]`); `test-b5-pin-guard.sh` stays green **unchanged** (its 37/37 literals remain true); the live-nats suite's planned `bus-priority-seam`/`bus-audit-events` slots and 37→39 re-baseline are **superseded with the seam** and must not land.
- **G6 gate honesty** (BR6): `implementation-gate.md:78` is verified accurate as-is (no bus legs named, no change required); each G6 component maps to its real landed verdict source.

**Out of scope**: wiring any part of the seam (BR1 forbids it); any change to the landed connector/relay/storage code (`pg.rs`, `relay.rs`, `config.rs`, `audit_governance.rs`, 0239/0240/0241); the T-11 drill body and the moderation-priority drill body (unchanged, E13); L1 aggregation mechanics and the `audit_outbox_frame` child-table DDL (dependency-owned, [PROPOSED], never landed — see R-5); the IM `event_outbox` claim/ordering invariants; `NotifyBatch` fan-out; the v1 snaplink usage relay; the out-of-repo 22 `[PROPOSED]` contract slots.

## 4. Requirements

### BR1 — Supersede-or-wire decision: SUPERSEDE (normative, load-bearing)
- The aero-bus B5 NATS transport seams are **superseded** by the landed PG-claim relay. Normative statement (recorded in this spec and referenced by the seven docs via BR2): *"The audit-governance delivery path is `audit_governance_outbox` (0239) → `PgOutboxRepo::claim_due` (ORDER BY priority DESC, pg.rs:117; index 0240) → `AuditRelay` HTTP POST (relay.rs) → settle/requeue/mark_dead; 403→dead is T-11; moderation priority is claim ordering. No aero-bus NATS transport (AUDIT_EVENTS on `audit.ws.*`, AUDIT_PRIORITY on `audit.priority.*`, or any publish path from aero-bus to an audit subject) will be built."*
- **Single invariant (A5/T-11 by construction)**: `crates/aero-bus` contains **no publish path to any audit subject** the sink could consume without the connector's claim/provision gates — enforced by the grep guard (A1): `rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0. This is the *supersede* form of the b5-3 spec's A5 constraint test: instead of "the new stream is transport-only", there is no stream at all.
- The decision does **not** undo the landed claim-level priority semantics (E6/E11/E17) nor the T-11 connector behavior (E7) — those remain the delivery path and are referenced, not modified.

### BR2 — Superseded pointers in all seven aero-bus B5 docs
- Each of the following docs carries an explicit, grep-able pointer with the verbatim sentinel `superseded by PgOutboxRepo::claim_due priority DESC` (plus the one-line rationale: the landed PG-claim relay delivers moderation priority via claim ordering and T-11 via connector 403→dead; G6 is satisfiable without a bus hop; wiring would create a redundant second delivery path that must not bypass the connector's claim/provision gates — b5-3 A5):
  1. `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md`
  2. `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md`
  3. `docs/requirements/2026-08-07-aero-bus-live-nats-acceptance-suite.req.md`
  4. `docs/requirements/2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` (the "land the seam" spec — the pointer is its decision record)
  5. `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md`
  6. `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md`
  7. `docs/design/2026-08-07-aero-bus-live-nats-acceptance-suite.design.md`
- Placement: a **Status** line addition near the top (e.g. `**Status**: Superseded — see `docs/requirements/2026-08-08-aero-bus-b5-reconcile-pg-claim-relay.req.md` (BR1)`) and a one-paragraph **Supersession note** in §Scope. The docs' historical content (API shapes, acceptance, drill mappings) is preserved verbatim — the pointer states the decision, it does not rewrite history.
- What survives the supersession (must be stated in each pointer where relevant): b5-3's **claim-level** priority semantics (DR1/DR2/DR3 → landed at pg.rs:117/0240), b5-1's status-machine semantics (DR4 → landed in `AuditRelay`/`PgOutboxRepo`), T-11 fail-closed (A5 → connector 403→dead), the moderation-priority drill mapping (A3 → `moderation-priority-drill`), and BR6 doc alignment (→ BR4 of this spec).

### BR3 — Close handoff H1 (closure record)
- **Closure criterion** (precise): the b5-3 aero-bus design doc carries **no normative "DEFAULT 0"** for the governance `priority` column — its normative pin is `DEFAULT 10` = `GOVERNANCE_PRIORITY_BACKLOG` (governance.rs:33), matching the landed 0239 DDL (`priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`) and the behavioral db_test `ddl_contract_defaults_and_checks` (`crates/aero-storage/src/audit_governance.rs:425`). The two remaining "DEFAULT 0" strings at b5-3 design doc :144/:193 are the **reconciliation annotations themselves** (they narrate the draft's superseded value) — they are evidence of closure, not open debt.
- Record the closure: 0239's H1 comment ("B5-3's aero-bus design doc must reconcile its 'DEFAULT 0' to 10 at landing") is **discharged as of 2026-08-08**; the second H1 half (0240 due-prio index "is B5-3's, not this slice's") is also discharged — the index exists with the exact `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` ORDER BY shape (E11).
- Precision note: the 08-08 verified-landing design doc's closure check (":158 — `grep 'DEFAULT 0' …` must be empty") is superseded by this spec's criterion; the check as literally written cannot pass because the annotations legitimately quote the draft value (R-3).

### BR4 — `crates/aero-bus/src/lib.rs` stream doc lists all 4 declared streams (BR6)
- The crate doc's stream list (lib.rs:9-13) is rewritten to match `bootstrap()` exactly: `IM_MESSAGES` (`im.room.*`, limits, 7d, file) · `IM_EVENTS` (`im.events.>`, limits, 30d, file) · `AI_QUEUE` (`ai.queue.*`, work-queue, 1d) · `LIVE_EVENTS` (`live.stream.*`, limits, 6h, file, max 200k). The stale "Implementation is filled in by the storage/bus agent" tail line is removed (the implementation exists — this file is it).
- Invariant stated in the doc: the list must match `bootstrap()`'s `get_or_create_stream` declarations (4 today; a future stream addition must extend the doc in the same change).

### BR5 — b5-pin honesty: manifest stays 37, no bus-backed executed slot
- `scripts/b5-pin.sh` `B5_CONTRACT_TEST_LIST` **stays exactly 37 slots** (15 executed + 22 `[PROPOSED]`) — no re-baseline, no added slots. The `-ne 37` guard literal, the "37 slots: 15 executed + 22 [PROPOSED]" header, and `scripts/test-b5-pin-guard.sh`'s literals are **unchanged** and stay green.
- The `bus-priority-seam` / `bus-audit-events` slots proposed by the live-nats acceptance suite (BR4 of that spec) are **not added** — they are superseded with the seam (BR1). If any future change touches the manifest, the honesty rule is: an executed slot's verdict source must be **landed code** (migration/db tests, connector drills, harness legs that run today) — never a doc-mandated-but-unlanded artifact.
- Verified mapping of the 15 executed slots to landed verdict sources is recorded in this spec (E12/E13/E14) — this is the honesty evidence the direction requires.

### BR6 — G6 gate honesty: `implementation-gate.md:78` is accurate as-is
- The G6 row "37/37、T-11、moderation 优先级" is verified accurate and requires **no change**: it names no bus legs, and each component's verdict source is landed — 37/37 (E12, 15 executed slots all backed by landed code), T-11 (t11-fail-closed leg + connector 403→dead, E7/E13), moderation 优先级 (moderation-priority-drill via claim ordering `priority DESC`, E6/E11/E13). The gate doc's G6 row gains no bus-leg mention.

## 5. Acceptance checks (preserved from the direction, made testable)

Ownership tags: **[aero-bus]** = code/doc in this module; **[connector]** = landed aero-audit-connector (unchanged, run as evidence); **[harness]** = scripts; **[docs]** = the seven docs + this spec.

### A1 — T-11 stays PASS; aero-bus contains no audit publish path **[aero-bus] + [connector] + [harness]**
*Preserves: "T-11: `cargo run -p aero-audit-connector --bin aero-audit-t11-drill` stays PASS and aero-bus contains no publish path to any audit subject the sink could consume without connector claim/provision gates (A5 grep guard, e.g. `rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0 or wired)."*
- **Assert 1 (grep guard)**: `rg -n 'audit\.(priority|ws)\.' crates/aero-bus` → **0 hits** (verified today, E3; this is the supersede form of the b5-3 A5 constraint test — no stream, no second delivery path). A future change that makes it non-zero must be a deliberate, gate-preserving wiring that re-proves the b5-3 A5 invariant — the default posture is 0.
- **Assert 2 (drill, standalone)**: on a throwaway migrated DB, `DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-t11-drill` exits 0 with the four T-11 invariants (COUNT(status=0)==N, COUNT(status IN (1,2,3))==0, SUM(attempts) N→2N across rounds, last_error carries the closed-endpoint fragment); exits 2 with a clear message when the 0239 table is absent.
- **Assert 3 (drill, in-harness)**: `scripts/test-integration.sh` fresh mode → B5 log contains `B5-CHECK t11-fail-closed: PASS` (:436, unchanged).
- **Assert 4 (no bus code)**: `crates/aero-audit-connector/Cargo.toml` has no `aero-bus` dependency (E5) — the connector's delivery path is PG→HTTP with no bus hop; `grep -c aero-bus crates/aero-audit-connector/Cargo.toml` = 0.

### A2 — 37/37 pin honest; guard self-test green **[harness]**
*Preserves: "37-tests: scripts/b5-pin.sh executed-slot verdict sources match reality (no executed slot backed by the unlanded seam; slot count stays 37 or is re-baselined with the guard test test-b5-pin-guard.sh updated)."*
- **Assert 1**: `B5_CONTRACT_TEST_LIST` = exactly 37 slots (15 executed + 22 `[PROPOSED]`), no duplicates, no malformed entries, ≥1 executed slot (non-vacuous).
- **Assert 2 (honesty)**: none of the 15 executed slot names is a bus-seam artifact; each executed slot's verdict source is landed code (migration-regression filters; `audit_governance::` + `moderation_finalize_outbox_parity` aero-storage db_test filters; `a3-relay-drill` / `t11-fail-closed` / `moderation-priority-drill` / `relay-mock-probe` / `audit-provision-check` / `notification-fanout` legs — E12/E13/E14).
- **Assert 3 (guard unchanged, green)**: `bash scripts/test-b5-pin-guard.sh` → exit 0, including the literals `B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS` (positive + SKIP_DB_CREATE cases).
- **Assert 4 (no re-baseline)**: no `bus-priority-seam` / `bus-audit-events` slot exists in `B5_CONTRACT_TEST_LIST` (the live-nats suite's 37→39 plan is superseded, BR1/BR5).

### A3 — Moderation priority stays PASS via claim ordering **[connector] + [storage] + [harness]**
*Preserves: "Moderation priority: G6 row 3 — `aero-audit-priority-drill` (500 backlog + 1 moderation, priority 100 vs 10) stays PASS via claim ordering."*
- **Assert 1 (claim ordering)**: `crates/aero-audit-connector/src/pg.rs` `claim_due` orders `priority DESC` (E6, :117) — moderation lane 100 precedes backlog lane 10 regardless of enqueue/available time; the 0240 partial index matches the ORDER BY exactly (E11). No change to either file.
- **Assert 2 (drill, standalone bin)**: on a throwaway migrated DB, `DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-priority-drill` exits 0: seeds 500 backlog rows (priority 10 = `GOVERNANCE_PRIORITY_BACKLOG`, enqueued FIRST) + 1 moderation row (class 'admin', priority 100 = `GOVERNANCE_PRIORITY_MODERATION`, enqueued LAST — so ordering evidence can only come from priority, never FIFO); asserts round-1 `COUNT(status=2)==100` **with the moderation row in that top-100 claimed set** (batch membership is the contract of `priority DESC` — delivery firstness is an executor artifact, D3) and full-drain `COUNT(status=2)==501` with event_id set-parity.
- **Assert 3 (drill, in-harness)**: `scripts/test-integration.sh` fresh mode (0239 present) → `aero-cli audit-provision-check --priority` on a throwaway DB exits 0 with the `priority: landed` verdict; B5 log contains `B5-CHECK moderation-priority-drill: PASS` (:459, unchanged).
- **Assert 4 (priority constants)**: `GOVERNANCE_PRIORITY_MODERATION=100 > GOVERNANCE_PRIORITY_BACKLOG=10` (governance.rs:31/:33) — DESC higher-first convention, single-sourced (E17).

### A4 — Superseded pointers present in 7/7 docs **[docs]**
*Preserves: "each aero-bus B5 doc either lands code or carries an explicit 'superseded by PgOutboxRepo::claim_due priority DESC' pointer."*
- **Assert 1**: `for f in docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md docs/requirements/2026-08-07-aero-bus-live-nats-acceptance-suite.req.md docs/requirements/2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md docs/design/2026-08-07-aero-bus-live-nats-acceptance-suite.design.md; do grep -q 'superseded by PgOutboxRepo::claim_due priority DESC' "$f" || echo "MISSING: $f"; done` → no MISSING lines (7/7).
- **Assert 2**: each pointer also names this spec (`2026-08-08-aero-bus-b5-reconcile-pg-claim-relay.req.md`) as the decision record.
- **Assert 3**: the docs' historical content (API shapes, acceptance lists, drill mappings) is preserved verbatim — the pointer is additive, not a rewrite.

### A5 — Handoff H1 closed **[docs] + [migration] + [storage]**
*Preserves: "close handoff H1" (0239 comment: "B5-3's aero-bus design doc must reconcile its 'DEFAULT 0' to 10 at landing").*
- **Assert 1 (normative pin)**: `grep -n 'priority SMALLINT NOT NULL DEFAULT' docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` → the design's normative line is `DEFAULT 10` (with the H1-reconcile annotation at :144/:193); no *normative* `DEFAULT 0` remains (the 2 remaining occurrences are the annotations themselves, quoting the draft).
- **Assert 2 (DDL)**: `migrations/0239_audit_governance_outbox.sql` → `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)` (E10).
- **Assert 3 (behavioral pin)**: `cargo test -p aero-storage --lib -- --ignored audit_governance::` (throwaway migrated DB) → `ddl_contract_defaults_and_checks` passes (audit_governance.rs:425) — the default is asserted behaviorally, not just textually.
- **Assert 4 (index half)**: `migrations/0240_audit_governance_due_prio_idx.sql` exists with `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` (E11) — H1's second half discharged.

### A6 — lib.rs lists all 4 declared streams; suite stays green **[aero-bus]**
*Preserves: "lib.rs doc lists all 4 declared streams."*
- **Assert 1**: `grep -c '^- \`' crates/aero-bus/src/lib.rs` = 4; each stream name (`IM_MESSAGES`, `IM_EVENTS`, `AI_QUEUE`, `LIVE_EVENTS`) appears in `bootstrap()`'s `get_or_create_stream`/`update_stream` calls in `jetstream.rs` (doc↔code parity, E1/E4).
- **Assert 2**: `cargo check --workspace` clean; `cargo test --workspace --lib` all green (PG-gated via `-- --ignored` + `DATABASE_URL` + throwaway migrated DB); `cargo clippy --workspace --all-targets` no new warnings; `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations (AGENTS.md §4.3 gate).
- **Assert 3**: `scripts/test-integration.sh` fresh mode → `assert_b5_contract_pin` prints `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS` with all 15 executed-slot verdict lines present (E12) — the harness closure unchanged.

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| A1-1 grep guard (`rg 'audit\.(priority\|ws)\.' crates/aero-bus` = 0) | one-liner (CI or harness); recorded in this spec | shell; today = 0 hits (E3) |
| A1-2 t11 drill standalone | `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` (existing, unchanged) | throwaway migrated DB + `DATABASE_URL` |
| A1-3 t11 harness leg | `scripts/test-integration.sh:436` (existing, unchanged) | G6 fresh mode; `B5-CHECK t11-fail-closed: PASS` |
| A1-4 no connector bus dep | `grep -c aero-bus crates/aero-audit-connector/Cargo.toml` | shell; today = 0 (E5) |
| A2-1..4 pin honesty + guard self-test | `scripts/b5-pin.sh` + `scripts/test-b5-pin-guard.sh` (existing, unchanged) | `bash scripts/test-b5-pin-guard.sh` → exit 0; fresh-mode closure in `test-integration.sh` |
| A3-1 claim ordering | `crates/aero-audit-connector/src/pg.rs:117` + `migrations/0240_…` (existing, unchanged) | code review + drill |
| A3-2 priority drill standalone | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` (existing, unchanged) | throwaway migrated DB + `DATABASE_URL`; 500×priority 10 + 1×priority 100 → moderation row in top-100 claimed set, 501 drained |
| A3-3 priority drill harness leg | `scripts/test-integration.sh:441-461` (existing, unchanged) | G6 fresh mode; `B5-CHECK moderation-priority-drill: PASS` |
| A4-1/2 superseded pointers | the seven docs (this direction's edit) | `grep -q 'superseded by PgOutboxRepo::claim_due priority DESC'` over the 7 files |
| A5-1 H1 normative pin | `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` (existing, annotated) | grep |
| A5-2 DDL default | `migrations/0239_audit_governance_outbox.sql` (existing, landed) | grep |
| A5-3 behavioral default | `crates/aero-storage/src/audit_governance.rs` db_test `ddl_contract_defaults_and_checks` (existing) | PG, `#[ignore]` + `DATABASE_URL` + throwaway migrated DB |
| A6-1 lib.rs doc parity | `crates/aero-bus/src/lib.rs` (this direction's edit) | `grep -c '^- \`'` = 4 + name parity with `bootstrap()` |
| A6-2/3 workspace gate | CI + `make migrate-smoke` + `scripts/test-integration.sh` | full suite; G6 |

## 7. Risks / [PROPOSED] items

- **R-1 — Future implementer wires the seam anyway**: the seven docs once mandated it; a fresh reader could re-derive it. Mitigation: BR1's normative statement + the A4 grep-able pointers (a doc edit that re-asserts the seam without the pointer fails A4). The hard invariant stays the b5-3 A5/T-11 constraint: **no audit subject may reach the sink without the connector's claim/provision gates** — the supersede form (zero audit subjects in aero-bus, A1-1) is strictly stronger than the wired form.
- **R-2 — The live-nats suite's 37→39 plan must not land**: its BR4 (manifest + guard literals re-baseline) and BR2 (two harness legs) are superseded with the seam (BR1/BR5). Landing them would add executed slots whose backing code (AUDIT_EVENTS/AUDIT_PRIORITY) does not exist — the exact honesty violation BR5 forbids.
- **R-3 — H1 closure-check precision**: the 08-08 verified-landing design doc's closure check (`grep 'DEFAULT 0' … must be empty`) is literally unsatisfiable because the reconciliation annotations quote the draft value (E16). This spec pins the precise criterion — **no normative DEFAULT 0** (A5-1) — and records that the two remaining strings are annotations. The verified-landing doc's check line should be read (or annotated) per this criterion, not "fixed" by deleting the annotations (they are the closure evidence).
- **R-4 — Pointer drift**: the seven docs are a bounded set; a future rename/reorg must preserve the sentinel. A4's grep is the drift detector. The sentinel text is deliberately stable (`superseded by PgOutboxRepo::claim_due priority DESC`) so `rg -l` over `docs/requirements docs/design` finds the whole superseded set in one command.
- **R-5 — L1 aggregation transport half is unlanded and stays unbuilt**: the exactly-N expansion contract, `audit_outbox_frame` child-table DDL, and per-subject seq composition (b5-1 BR5/DR5, transport-seam spec BR5) were the seam's raison d'être and are [PROPOSED]/dependency-owned with zero code (E15; `audit_outbox_frame` exists only in design/req docs). Superseding the seam supersedes the transport half too; if L1 aggregation ever lands, its output must fit the **landed** 0239 row shape (payload JSONB carrying the constituents) — no frame table, no expansion publish, no seq stamping is implied by this decision. Not built here.
- **R-6 — Evidence corrections**: (1) the 08-08 transport-seam spec is included in the superseded set — it is the "land the seam" spec, and this direction is its decision record; (2) the `audit_governance::` executed slot is the aero-storage db_test filter (landed `audit_governance.rs`), not a bus artifact; (3) line anchors (`pg.rs:117`, `lib.rs:9-13`, `test-integration.sh:436/:441-461`, `b5-pin.sh:23-46`) are as-of-verification — the file/symbol is the stable anchor per AGENTS.md §0.

## 8. Sequencing

1. **BR4 (lib.rs doc fix)** — the only `crates/aero-bus/src` change: rewrite the stream list to 4 + drop the stale tail line; `cargo check -p aero-bus` clean. Lands independently, immediately.
2. **BR2/BR3 (doc pointers + H1 closure record)** — docs-only: add the superseded sentinel + this-spec reference to the seven docs; append the H1 closure record to this spec's verification (§1 E16) and to the b5-3 design doc's H1-adjacent lines if not already annotated.
3. **BR5/BR6 (pin + gate honesty)** — no code: verify the 37-slot manifest, the unchanged guard self-test, and the G6 row; record the executed-slot→landed-source mapping (E12/E13/E14) as the honesty evidence.
4. **A1/A3 drill runs** — existing harness legs, unchanged: run `scripts/test-integration.sh` fresh mode (throwaway DBs + NATS reachable) to confirm `t11-fail-closed` PASS, `moderation-priority-drill` PASS, and the 37/37 pin closure; G6 (B5) is green with the same three components it required before this direction — the reconciliation changes the *documentation of the path*, not the path.
