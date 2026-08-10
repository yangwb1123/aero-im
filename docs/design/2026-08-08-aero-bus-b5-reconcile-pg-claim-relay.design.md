# Design — Reconcile the aero-bus B5 transport seams with the landed PG-claim relay: SUPERSEDE (decision record)

- **Module**: `crates/aero-bus/src` (one production-file doc fix) + seven in-repo aero-bus B5 requirement/design docs (superseded pointers) + harness verification surfaces (`scripts/b5-pin.sh`, `scripts/test-b5-pin-guard.sh`, `scripts/test-integration.sh` — unchanged)
- **Source**: `docs/requirements/2026-08-08-aero-bus-b5-reconcile-pg-claim-relay.req.md` (BR1–BR6, A1–A6, R-1..R-6)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)

## 0. Independent evidence verification (this design re-checks the req spec's 20 items, 2026-08-08)

| # | Claim (req spec) | Re-check verdict | Evidence |
|---|---|---|---|
| E1 | `bootstrap()` declares 4 streams, no audit stream | ✅ | `jetstream.rs` `get_or_create_stream` ×4 at :241/:254/:267/:283 — `IM_MESSAGES` (im.room.\*, Limits, 7d), `IM_EVENTS` (im.events.\>, Limits, 30d), `AI_QUEUE` (ai.queue.\*, WorkQueue, 1d), `LIVE_EVENTS` (live.stream.\*, Limits, 6h, 200_000, File) |
| E2 | `subscribe()` inline 4-prefix map, fail-closed unknown prefix | ✅ | `jetstream.rs` ~:343–353: `starts_with` chain over `im.room.`/`im.events.`/`ai.queue.`/`live.stream.`, else `Err(BusError::Nats("unknown subject prefix: …"))` |
| E3 | `rg 'audit\.(priority\|ws)\.' crates/aero-bus` = 0 hits | ✅ | 0 hits (exit 1) |
| E4 | `lib.rs:9-13` stale 3-stream doc, missing `LIVE_EVENTS`, stale tail line | ✅ | `lib.rs` lists 3 streams; `//! Implementation is filled in by the storage/bus agent.` at :13 |
| E5 | connector Cargo has no aero-bus dep | ✅ | `grep -c aero-bus crates/aero-audit-connector/Cargo.toml` = 0 |
| E6 | `pg.rs:117` claim `ORDER BY priority DESC … FOR UPDATE SKIP LOCKED` | ✅ | `pg.rs:117-119` exactly; `STATUS_ENQUEUED` at :34 from `OutboxStatus` discriminants 0..3 (aero-common leaf) |
| E7 | relay poll loop: claim → `client.deliver` → settle / requeue / 403→dead | ✅ | `relay.rs`: `run()`→`dispatch_batch()`(:164)→`claim_due`(:171)→`deliver_claim`(:182)→`client.deliver`(:186); success→`settle`; transient→fenced `requeue`; **403→`mark_dead` immediately** (T-11, no requeue); `PERMANENT_DEAD_AT=2` (:42) |
| E8 | `config.rs` `from_env` fail-loud | ✅ | presence-gated on `AERO_AUDIT_TOKEN_ENDPOINT`; stray `AERO_AUDIT_*` without it = boot error (:58-71) |
| E9 | `main.rs:245-261` relay wiring | ✅ (line drift) | `crates/aero-server/src/bin/main.rs:251-260` — `RelayConfig::from_env()` → `PgOutboxRepo::new` + `AuditClient` + `AuditRelay::new` + `tracker.spawn(relay.spawn(ai_shutdown))` |
| E10 | 0239: status 0/1/2/3, `priority DEFAULT 10`, H1 comment | ✅ | `migrations/0239_audit_governance_outbox.sql:30-37`; H1 comment at :10-12 |
| E11 | 0240 due-prio index matches ORDER BY; 0241 landed | ✅ | `0240_…(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`; `0241_governance_reconcile.sql` present |
| E12 | manifest 37 slots (15 executed + 22 `[PROPOSED]`) | ✅ (see nuance) | `b5-pin.sh` `B5_CONTRACT_TEST_LIST` = 15 executed + 22 `contract-test-NN[PROPOSED]`; guard `-ne 37` at :87. **Nuance**: the array contains one comment line (`# Out-of-repo contract slots — …`) which naive line parsers count as a 38th element; bash does not (a `#`-leading word inside `( )` is a comment). Slot count = 37, guard passes. |
| E13 | `test-integration.sh:436` t11 PASS; :441-461 moderation-priority drill | ✅ (line drift) | `B5-CHECK t11-fail-closed: PASS` and `B5-CHECK moderation-priority-drill: PASS` blocks present; drill greps `priority: landed` verdict |
| E14 | drill bins exist; NATS URL export | ✅ (line drift) | `src/bin/{aero-audit-t11-drill,aero-audit-priority-drill,aero-audit-relay-drill}.rs`; `export AERO__NATS__URL=…` at `test-integration.sh:512` (spec cited :489 — same symbol) |
| E15 | seven docs mandate the seam; no harness legs in scripts | ✅ | all 7 files exist; `rg 'bus-priority-seam|bus-audit-events' scripts/` = 0 hits |
| E16 | b5-3 design: 2 `"DEFAULT 0"` hits, both annotations | ✅ | `grep -n 'DEFAULT 0' docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` = :144 (narrates draft value) + :193 (H1-reconcile annotation); normative pin `DEFAULT 10` at :145 |
| E17 | governance priority constants | ✅ | `governance.rs:31` = 100, `:33` = 10; DESC convention documented at :13 |
| E18 | guard self-test literals | ✅ | `test-b5-pin-guard.sh:64/:113` `B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS` |
| E19 | G6 gate row accurate | ✅ | `implementation-gate.md:78` `\| G6（B5） \| B5-1..4 \| 37/37、T-11、moderation 优先级 \|` |
| E20 | NATS knob + blanket `--ignored` live-NATS coverage | ✅ | `test-integration.sh:17` comment + `:512` export; Step 4 `cargo test --workspace --lib --locked -- --ignored --test-threads=1` |

**Additional verification beyond the req spec** (no contradictions found):
- The connector-side `docs/requirements/2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.req.md` mentions `audit.priority.*` **only to exclude it** (its :48 lists the seam as "separate" out-of-scope) — it correctly is **not** in the 7-doc superseded set.
- The R-3 premise is real: `docs/design/2026-08-08-aero-ai-b5-1-migration-0239-verified-landing.design.md:158` pins the closure check as `grep 'DEFAULT 0' … must be empty`, which is literally unsatisfiable (the annotations quote the draft). This design extends BR3 to annotate that check line (step M2.4).

**Verdict**: req spec is accurate; zero fabrication. The direction is executable as specified with the additions recorded below.

## 1. Design overview

```
DECISION D0 (normative): SUPERSEDE. No AUDIT_EVENTS / AUDIT_PRIORITY stream,
no audit.ws.* / audit.priority.* subjects, no stream_for_subject extraction,
no AUDIT_SUBJECT_PREFIX / AUDIT_PRIORITY_SUBJECT_PREFIX constants, no
exactly-N expansion, no connector bus dependency — nothing of the seam is built.
```

Landed delivery path (unchanged, referenced not modified):

```
audit_events (L0) → 0239 trigger aero_enqueue_governance_audit (class 'admin',
                    priority 100, envelope 'admin.content.flag') + 0241 reconciler
  → audit_governance_outbox (0239: status 0/1/2/3, priority DEFAULT 10 CHECK(>0),
                             lease/attempts/backoff; 0240: due-prio partial index)
  → PgOutboxRepo::claim_due  ORDER BY priority DESC  (pg.rs:117)   ← moderation priority
  → AuditRelay::run → dispatch_batch → client.deliver (HTTP POST, Idempotency-Key=event_id)
  → settle(2) / requeue(transient, permanent-1) / mark_dead(403=T-11, permanent ≥2)
```

Forbidden seam (docs-only, superseded with this design):

```
audit.priority.* → AUDIT_PRIORITY (WorkQueue) → connector durable subscribe → sink   ✗
audit.ws.*       → AUDIT_EVENTS (Limits, 7d)    → exactly-N expansion + per-subject seq ✗
bus-priority-seam / bus-audit-events harness legs + 37→39 re-baseline                ✗
```

**Why SUPERSEDE wins** (requirement-level table in the req spec §2, adopted here): the PG outbox is already the durable queue; `claim_due` already orders priority; `settle`/`requeue`/`mark_dead` already implement the status machine; 403→dead already provides T-11. Wiring adds a second delivery path whose publisher would have to be a third component re-proving lease fencing, dedup, seq stamps, and WorkQueue ack semantics for zero added delivery guarantee — and any mistake in that path risks bypassing the connector's claim/provision gates (b5-3 A5).

**A5/T-11 by construction**: the supersede form of the b5-3 A5 constraint test is *stronger* than the wired form — instead of "the new stream is transport-only", there is no audit subject in aero-bus at all (`rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0). The sink cannot consume anything without the connector's gates because there is no bus path to the sink.

## 2. API changes

### 2.1 Production code — exactly one file, doc-only

`crates/aero-bus/src/lib.rs` crate doc (lines 8-13) is rewritten to:

```rust
//! Streams declared (must match `bootstrap()` in `jetstream.rs` — a future
//! stream addition extends both in the same change):
//! - `IM_MESSAGES`  subjects = `im.room.*`            retention=limits, 7d, file
//! - `IM_EVENTS`    subjects = `im.events.>`          retention=limits, 30d, file
//! - `AI_QUEUE`     subjects = `ai.queue.*`           retention=work-queue, 1d
//! - `LIVE_EVENTS`  subjects = `live.stream.*`        retention=limits, 6h, file, max 200k
```

The stale tail line `//! Implementation is filled in by the storage/bus agent.` is deleted (the implementation exists — this file is it).

**Public API surface: zero additions, zero removals, zero signature changes.** `EventBus` trait (`traits.rs`), `JetStreamBus`/`JetStreamConfig` (`jetstream.rs`), `seq` (`seq.rs`) are untouched. `bootstrap()` still declares exactly the 4 existing streams; `subscribe()` still fails closed on unknown prefixes.

### 2.2 Docs — the seven superseded docs + one annotation

Each of the seven docs (A4 list, §6 below) gains, near the top:
1. A `**Status**: Superseded — see `docs/requirements/2026-08-08-aero-bus-b5-reconcile-pg-claim-relay.req.md` (BR1)` line, and
2. a one-paragraph **Supersession note** containing the verbatim sentinel `superseded by PgOutboxRepo::claim_due priority DESC` plus the one-line rationale (PG-claim relay delivers moderation priority via claim ordering and T-11 via connector 403→dead; G6 is satisfiable without a bus hop; wiring would create a redundant second delivery path that must not bypass the connector's claim/provision gates — b5-3 A5).

What survives the supersession, stated per doc where relevant: b5-3 claim-level priority semantics (DR1/DR2/DR3 → landed `pg.rs:117`/0240), b5-1 status-machine semantics (DR4 → landed `AuditRelay`/`PgOutboxRepo`), T-11 fail-closed (A5 → connector 403→dead), the moderation-priority drill mapping (A3 → `moderation-priority-drill`), BR6 doc alignment (→ this design's BR4).

`docs/design/2026-08-08-aero-ai-b5-1-migration-0239-verified-landing.design.md:158` gains a one-line annotation: the H1 closure criterion is "no **normative** DEFAULT 0"; the two remaining occurrences at b5-3 :144/:193 are the reconciliation annotations themselves (R-3). Do not delete them — they are the closure evidence.

### 2.3 Explicit non-changes (compatibility fence)

| Artifact | Change? |
|---|---|
| `crates/aero-audit-connector/{pg,relay,config,client,fake,stub,outbox}.rs`, drills | **No** — untouched, run as evidence |
| `migrations/0239/0240/0241` | **No** — untouched; 0239's FIFO index deliberately kept (rolling deploy), legacy drop is later cleanup (D5) |
| `crates/aero-storage/src/audit_governance.rs` + db_tests | **No** |
| `crates/aero-ai/src/governance.rs` | **No** |
| `scripts/b5-pin.sh` (37 slots), `scripts/test-b5-pin-guard.sh` (37/37 literals) | **No** — unchanged and stay green |
| `scripts/test-integration.sh` legs (`t11-fail-closed`, `moderation-priority-drill`, `audit-provision-check`, `a3-relay-drill`, `relay-mock-probe`) | **No** |
| `docs/campaigns/implementation-gate.md:78` G6 row | **No** — verified accurate as-is |
| `crates/aero-bus/Cargo.toml` | **No** — no new deps |

## 3. Compatibility constraints

- **C1 (A5 guard)**: `rg 'audit\.(priority|ws)\.' crates/aero-bus` must return 0 hits. Any future change making it non-zero must be a deliberate, gate-preserving wiring that re-proves the b5-3 A5 invariant; the default posture is 0 (A1-1).
- **C2 (connector isolation)**: `grep -c aero-bus crates/aero-audit-connector/Cargo.toml` = 0 — the connector's delivery path is PG→HTTP with no bus hop (A1-4).
- **C3 (manifest)**: `B5_CONTRACT_TEST_LIST` stays exactly 37 elements (15 executed + 22 `[PROPOSED]`); no `bus-priority-seam`/`bus-audit-events` slots; `assert_b5_contract_pin`'s `-ne 37` and `test-b5-pin-guard.sh`'s `37/37 (15 executed, 22 [PROPOSED]): PASS` literals stay true (A2). If a future change legitimately re-baselines, the guard self-test must be updated in lockstep (E18).
- **C4 (landed path frozen)**: no edits to 0239/0240/0241, `pg.rs`, `relay.rs`, `config.rs`, `audit_governance.rs`, `governance.rs` in this change.
- **C5 (rolling deploy)**: 0239's FIFO due index and 0240's priority index coexist; nothing drops indexes or columns.
- **C6 (gate honesty)**: G6 row names no bus legs; each component's verdict source is landed code.
- **C7 (history preservation)**: the seven docs' historical content (API shapes, acceptance lists, drill mappings) is preserved verbatim — pointers are additive, not rewrites.
- **C8 (doc↔code parity)**: the lib.rs stream list must match `bootstrap()`'s declarations exactly (4 today); a future stream addition extends the doc in the same change as the code (A6-1).

## 4. Failure modes

| # | Failure | Tripwire / detection | Mitigation |
|---|---|---|---|
| F1 | Future implementer re-wires the seam (R-1) | A4 sentinel grep over the 7 docs; A1-1 grep over `crates/aero-bus` | BR1 normative statement + A4 pointers; a doc edit re-asserting the seam without the pointer fails A4; the hard invariant stays the b5-3 A5/T-11 constraint |
| F2 | live-nats suite's 37→39 plan lands (R-2) | `test-b5-pin-guard.sh` positive case fails (its 37/37 literal stops matching); `-ne 37` guard trips | BR5 normative prohibition + A2-4; executed slots must have landed-code verdict sources only |
| F3 | H1 closure check misread as unsatisfiable (R-3) | verified-landing doc :158 literal `grep 'DEFAULT 0' … must be empty` can never pass | M2.4 annotation pins the criterion "no normative DEFAULT 0"; the 2 remaining strings are annotations = closure evidence, not debt |
| F4 | Pointer drift on rename/reorg (R-4) | A4's `grep -q 'superseded by PgOutboxRepo::claim_due priority DESC'` over the 7 files; `rg -l` finds the whole superseded set in one command | Stable sentinel text; bounded 7-file set |
| F5 | L1 aggregation transport half resurrected (R-5) | any `audit_outbox_frame` DDL or expansion-publish code appearing | Superseding the seam supersedes the transport half; L1 output must fit the landed 0239 row shape (payload JSONB) — no frame table, no expansion publish, no seq stamping implied |
| F6 | Slot-count misread (new, found in verification) | naive parsers count the comment line inside `B5_CONTRACT_TEST_LIST=( )` as a 38th element | bash treats `#`-leading words in array literals as comments; the guard counts 37. Do not convert that comment to a non-comment line without re-checking the guard |
| F7 | Line-anchor drift in citations | `pg.rs:117`, `lib.rs:9-13`, `test-integration.sh:436/:512`, `main.rs:251-260` shift on edit | AGENTS.md §0 rule: file/symbol is the stable anchor, line numbers are as-of-verification |
| F8 | Doc-only change accidentally touching code | `cargo check --workspace` + clippy + truth-check in the gate | §6 A6-2/3; the only production edit is the lib.rs doc block |

## 5. Migration steps

No schema migration, no runtime rollout, no NATS stream provisioning — the change is one doc-comment edit + seven doc pointers + verification runs. Sequencing (per req spec §8, with the R-3 annotation folded in):

- **M1 — BR4 (lib.rs doc fix)**: apply §2.1 rewrite to `crates/aero-bus/src/lib.rs`; `cargo check -p aero-bus` clean; `cargo check --workspace` clean. Lands independently, immediately.
- **M2 — BR2/BR3 (doc pointers + H1 closure)**:
  - M2.1: add Status line + Supersession note (sentinel verbatim) to the **four req docs** (A4 list items 1-4).
  - M2.2: add the same to the **three design docs** (A4 list items 5-7).
  - M2.3: append the H1 closure record to the b5-3 design doc's H1-adjacent lines if not already annotated (its :145/:193 already carry the H1-reconcile annotation — verify, don't duplicate).
  - M2.4: annotate `docs/design/2026-08-08-aero-ai-b5-1-migration-0239-verified-landing.design.md:158` per R-3 (criterion = no normative DEFAULT 0; the two strings at b5-3 :144/:193 are the annotations).
- **M3 — BR5/BR6 (pin + gate honesty, no code)**: confirm `b5-pin.sh` = 37 slots, `test-b5-pin-guard.sh` green unchanged, G6 row at `implementation-gate.md:78` accurate; the executed-slot→landed-source mapping (req spec E12/E13/E14) is the honesty evidence.
- **M4 — Acceptance runs (A1/A3 drills, existing harness legs)**: fresh-mode `scripts/test-integration.sh` on throwaway DBs (NATS reachable) → `B5-CHECK t11-fail-closed: PASS`, `B5-CHECK moderation-priority-drill: PASS`, `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`; plus standalone drills A1-2/A3-2. G6 (B5) stays green with the same three components it required before — the reconciliation changes the documentation of the path, not the path.

Commit hygiene: the working tree currently carries a large in-flight change set; this change is docs + one doc-comment, and must not be folded into unrelated refactors (AGENTS.md §4.1). `git status` must show exactly `crates/aero-bus/src/lib.rs` (prod) + the doc files (M2) when this change is isolated.

## 6. Testable acceptance mapping

| Acceptance | Concrete command / check | Expected result |
|---|---|---|
| **A1-1** grep guard (supersede form of A5) | `rg -n 'audit\.(priority\|ws)\.' crates/aero-bus` | exit 1, 0 hits |
| **A1-2** T-11 drill standalone | `DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-t11-drill` (throwaway migrated DB) | exit 0: `COUNT(status=0)==N`, `COUNT(status IN (1,2,3))==0`, `SUM(attempts)` N→2N, `last_error` carries closed-endpoint fragment; exit 2 with clear message if 0239 table absent |
| **A1-3** T-11 harness leg | `scripts/test-integration.sh` fresh mode | `B5-CHECK t11-fail-closed: PASS` in B5 log |
| **A1-4** no connector bus dep | `grep -c aero-bus crates/aero-audit-connector/Cargo.toml` | `0` |
| **A2-1/2** pin honesty | `scripts/b5-pin.sh` manifest inspection | exactly 37 slots (15 executed + 22 `[PROPOSED]`), no dupes/malformed, ≥1 executed; none of the 15 executed names is a bus-seam artifact; verdict sources are landed code (E12/E13/E14 mapping) |
| **A2-3** guard self-test unchanged | `bash scripts/test-b5-pin-guard.sh` | exit 0, incl. positive + SKIP_DB_CREATE `37/37 (15 executed, 22 [PROPOSED]): PASS` cases |
| **A2-4** no re-baseline | `grep -cE 'bus-priority-seam\|bus-audit-events' scripts/b5-pin.sh` | 0 |
| **A3-1** claim ordering | code review: `pg.rs:117` `ORDER BY priority DESC` + `migrations/0240` index shape | unchanged, match exactly |
| **A3-2** priority drill standalone | `DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-priority-drill` (throwaway migrated DB) | exit 0: 500×priority 10 enqueued first + 1×priority 100 enqueued last; round-1 `COUNT(status=2)==100` **with the moderation row in the top-100 claimed set** (batch membership is the `priority DESC` contract, not delivery firstness); full drain 501 with event_id set-parity |
| **A3-3** priority drill harness leg | `scripts/test-integration.sh` fresh mode (0239 present) | `aero-cli audit-provision-check --priority` exit 0 + `priority: landed` verdict; `B5-CHECK moderation-priority-drill: PASS` |
| **A3-4** priority constants | `grep -n 'GOVERNANCE_PRIORITY' crates/aero-ai/src/governance.rs` | MODERATION=100 at :31 > BACKLOG=10 at :33 |
| **A4-1** pointers 7/7 | `for f in <7 files>; do grep -q 'superseded by PgOutboxRepo::claim_due priority DESC' "$f" \|\| echo "MISSING: $f"; done` | no MISSING lines |
| **A4-2** decision record named | `grep -l '2026-08-08-aero-bus-b5-reconcile-pg-claim-relay' <7 files>` | 7/7 |
| **A4-3** history preserved | `git diff` on the 7 docs | additions only (Status line + note), no deletions of historical content |
| **A5-1** normative pin | `grep -n 'priority SMALLINT NOT NULL DEFAULT' docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` | `DEFAULT 10` at :145; the only 2 `DEFAULT 0` strings are annotations (:144/:193) |
| **A5-2** DDL default | `grep -n 'priority.*DEFAULT' migrations/0239_audit_governance_outbox.sql` | `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)` |
| **A5-3** behavioral pin | `cargo test -p aero-storage --lib -- --ignored audit_governance::` (throwaway migrated DB) | `ddl_contract_defaults_and_checks` (`audit_governance.rs:425`, asserts `priority DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG` at :450) passes |
| **A5-4** index half | `head migrations/0240_audit_governance_due_prio_idx.sql` | `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` |
| **A6-1** lib.rs doc parity | `grep -c '^- \`' crates/aero-bus/src/lib.rs` + name parity | `4`; each of `IM_MESSAGES`/`IM_EVENTS`/`AI_QUEUE`/`LIVE_EVENTS` appears in `bootstrap()`'s stream configs (`jetstream.rs`) |
| **A6-2/3** workspace gate | `cargo check --workspace` · `cargo test --workspace --lib` (PG-gated `-- --ignored` + `DATABASE_URL`) · `cargo clippy --workspace --all-targets` · `scripts/{truth-check,file-size-check,web-check}.sh` | all clean, no new warnings, 0 violations; fresh-mode `test-integration.sh` prints `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS` with all 15 executed verdict lines |

## 7. Out of scope (unchanged, recorded for completeness)

Wiring any part of the seam; the landed connector/relay/storage code; the T-11 and moderation-priority drill bodies; L1 aggregation mechanics and `audit_outbox_frame` DDL (dependency-owned, [PROPOSED], never landed — R-5); IM `event_outbox` claim/ordering invariants; `NotifyBatch` fan-out; the v1 snaplink usage relay; the out-of-repo 22 `[PROPOSED]` contract slots; the `aero-eng`/`aero-cli` drill plumbing (`audit-provision-check --priority`, `aero-audit-*` bins — existing, unchanged).
