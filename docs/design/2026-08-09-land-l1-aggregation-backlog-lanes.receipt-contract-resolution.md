# Resolution — receipt-contract deadlock (F15) + envelope arbitration + delivery-leg artifact

- **Applies to**: `docs/design/2026-08-09-land-l1-aggregation-backlog-lanes.design.md` (run `7c9b1ec4`), slice `land-l1-aggregation-for-the-message-room-backlog`
- **Resolves**: wire-contract adversarial F15 (`2026-08-08-l1-aggregation-wire-contract-leaf.adversarial-review.md` §1.2) + DB-reviewer items 1/2/4 (`2026-08-09-land-l1-aggregation-backlog-lanes.adversarial-review.md`) + security-reviewer B1/F15 + acceptance-reviewer gaps (a)/(b) — **all verified against the live connector code**, not the design text
- **Anchor files** (symbol anchors only, per AGENTS.md §0): `crates/aero-audit-connector/src/{pg,stub,relay,client,outbox,fake,config}.rs` · `crates/aero-audit-connector/src/bin/{aero-audit-relay-drill,aero-audit-t11-drill}.rs` · sibling `docs/requirements/2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.req.md` (R3/R8/AC4) + sibling design §2.4 SQL · leaf contract spec `docs/auto/specs/crates-aero-common-src-630e5499-l1-aggregation-wire-contract.md` (R4/R5)

## 1. Full outbox lifecycle — source-verified traces (enqueue → claim → deliver → echo → settle)

The connector state machine, verbatim from the tree:

| Stage | Code (live) | Semantics that matter for window/spill rows |
|---|---|---|
| **Claim** | `pg.rs` `claim_due` | `WHERE status IN (0,1)` + due/lease filter; `ORDER BY priority DESC, available_at, created_at, event_id`; `FOR UPDATE SKIP LOCKED`; `UPDATE … SET status=1, claim_token=gen_random_uuid(), lease=clock_timestamp()+lease, attempts=attempts+1 … RETURNING outbox.event_id, …, **outbox.payload**, priority, class`. **The payload is a frozen snapshot at claim time** — the POST body (`client.rs` sends `claim.payload`) is that snapshot. |
| **Deliver** | `client.rs` `deliver` | ① `validate_delivery_payload` **first**: payload must carry **no `tenant_id`** and `payload.source_system == config.source_system` (`AERO_AUDIT_SOURCE_SYSTEM`, required env, `config.rs`); failure = `Permanent(PayloadGuard)`. ② `Idempotency-Key: claim.event_id.to_string()` = `AuditId` Display (**ULID base32**, `outbox.rs` doc; any 128-bit UUID via the total `AuditId::from_uuid`). ③ 202 → `validate_audit_receipt` → `receipt_event_id_matches`: parse echo as UUID first, else base32, compare **value-level against `claim.event_id`** (the row's own PK). Failure = `Permanent(ReceiptMismatch)`. |
| **Echo source** | `stub.rs` `handle_connection` (`/events`) | The in-repo sink echoes **`payload.event_id`** (`request.body` → `.get("event_id")` → string); absent → echo `"missing"`. `receipt_valid` knob only corrupts the echo; the **source field is hardwired to `payload.event_id`**. |
| **Settle** | `pg.rs` `settle` | Fenced re-read (`event_id AND claim_token AND status IN (0,1) AND lease_expires_at > clock_timestamp() FOR UPDATE`) then `SET status=2, delivered_at, claim_token=NULL, lease_expires_at=NULL, last_error=NULL`. **Never re-reads payload** — once settled, what the sink ledger holds is exactly the claim-time snapshot. |
| **Permanent class** | `relay.rs` `deliver_claim` + `is_dead_at` | `Permanent` → `is_dead_at(attempts)`: attempt 1 → `requeue`, attempt ≥ 2 → `mark_dead` (**dead after ≤1 retry**; `PERMANENT_DEAD_AT = 2`). 403 → immediate `mark_dead`. Transient → `requeue` forever. |

### 1.1 The deadlock, exactly

`stub.rs` echoes `payload.event_id`; `receipt_event_id_matches` compares against `claim.event_id` (row PK). Therefore the **load-bearing condition for every row shape is `payload.event_id` == row `event_id` (uuid value)**. Consequences, verified per shape:

| Row shape | Payload as designed before this resolution | Receipt outcome |
|---|---|---|
| 1:1 (0239 moderation) | `event_id = NEW.id::text` = row PK ✓ (`0239` trigger) | echo = PK → UUID parse → `AuditId::from_uuid` == `claim.event_id` ✓ **settles** (proven today by `aero-audit-relay-drill.rs` `COUNT(status=2)==N`) |
| Window (0242) | leaf envelope has **`window_id`, no `event_id`** (leaf R4 field table); land design §2.1/§2.3 sketch also lacks `event_id` | echo `"missing"` → UUID parse fails, base32 parse fails → `ReceiptMismatch` → permanent → **dead after ≤1 retry, never delivered** |
| Spill (0242) | sibling SQL writes `'event_id', v_event_id::text` = **window key**, not the spill row's own PK (`gen_random_uuid()` in sibling; deterministic key in land design) | echo = window key ≠ spill row PK → `ReceiptMismatch` → permanent → **dead after ≤1 retry** |

**New finding beyond F15 (not flagged by any prior reviewer)**: the sibling's and the land design's envelopes **both omit `source_system`**, and `validate_delivery_payload` (`client.rs`, runs *before* the POST) rejects any payload whose `source_system` ≠ relay config — `Permanent(PayloadGuard)`, also dead after ≤1 retry. As written, **every window and spill row dies at the payload guard with zero POSTs** — an earlier gate than the receipt echo. The 1:1 path gets `source_system` from the binding table; L1 has no binding, so the trigger needs a pinned constant (see §3). The DB reviewer's "in-repo contract only requires `event_id` + `idempotency_key` (+ count)" is incomplete: **`source_system` is a third hard requirement** (`AERO_AUDIT_SOURCE_SYSTEM`, required env — the deployment must set it to the pinned value, the same single-value convention `snaplink_commercial/http.rs`, the drill configs, and the harness already use: `"aero-im.source"`).

## 2. Verdict: `WHERE status = 0` merge + FOUND-branching spill survive the lifecycle

Confirmed against the live state machine — the corrected trigger body is lifecycle-safe:

1. **Merge bound `status = 0` (restore sibling R3/D5; revert land design `IN (0,1)`)**. Trace: `claim_due` snapshots payload at claim and flips status 1; `settle` never re-reads payload. With `IN (0,1)`, an event arriving in the claim→settle window merges into the in-flight row → payload rewritten to count N+1 **after the sink already received the N snapshot** → row settles at N+1, sink ledger holds N — silent undercount on the common success path, invisible to the parity drill (outbox SUM == audit COUNT on both sides; only the sink has N). With `status = 0`: an in-flight (1) / delivered (2) / dead (3) row never matches → the DO UPDATE affects 0 rows → spill. The row lock + `FOR UPDATE SKIP LOCKED` on the claim side and the PK-lock on the merge side serialize both orders safely: merge-commits-before-claim ⇒ claim snapshots N+1; claim-commits-before-merge ⇒ WHERE false ⇒ spill. Either order delivers the event exactly once (window delivery or spill delivery), never both, never zero.
2. **Explicit branch, not silent no-op**: after the `ON CONFLICT DO UPDATE … WHERE status = 0` statement, the trigger must branch on the affected-row count **immediately**: `GET DIAGNOSTICS v_inserted = ROW_COUNT; IF v_inserted = 0 THEN <spill> END IF;` (sibling §2.4:227 precedent; PL/pgSQL `FOUND` is equivalent — `INSERT/UPDATE` sets it true iff ≥1 row affected — but `ROW_COUNT` is the explicit, sibling-consistent form). `ROW_COUNT = 0` ⟺ conflict-with-WHERE-false is exact: there is **no DELETE path** on `audit_governance_outbox` in production (all DELETEs are test cleanup), so a fresh insert always reports 1.
3. **Spill key = deterministic `md5(v_key || '|' || NEW.id::text)::uuid`** (land design) — strictly better than the sibling's `gen_random_uuid()`: a replayed audit INSERT recomputes the same key → `ON CONFLICT (event_id) DO NOTHING` → at-most-one spill row per (window, event), replay-idempotent. `'|'` cannot appear in UUID text or epoch digits; the input is unambiguous.
4. **Spill row then completes the full loop**: status 0, class `message`, priority 10 → claimed under `priority DESC` (slots under moderation/100 — E5 verified) → POSTed with `Idempotency-Key` = own PK base32 → stub echoes own PK → receipt matches → `settle` → status 2. No connector code change anywhere; the pins are producer-side (0242 SQL).
5. **Concurrent merge** (F5 pins, both reviewer-requested lines): the guarantee is **READ COMMITTED-specific** — the pool sets only `statement_timeout = '10000'` (no isolation override, `aero-storage` pool config); under READ COMMITTED the `DO UPDATE` re-evaluates SET/WHERE against the winner's committed row (EvalPlanQual), so queued waiters re-increment in turn. A future REPEATABLE READ/SERIALIZABLE move turns same-window merges into 40001 (message-tx abort — availability, not correctness), and a hot window can hold the PK lock up to the 10s statement timeout. Both lines go in the F5 row; the only artifact that pins EPQ directly is a **concurrent two-tx merge db_test** (two parallel txs, same window, commit both, count=2) — add it.

## 3. Envelope arbitration (the field set — one envelope, both shapes)

**Arbitration authority**: the in-repo connector contract — `event_id` + `idempotency_key` + `source_system` (+ `count` for the drill math). Everything else is decoration and is arbitrated to the set below. The leaf wire-contract's 19-field `AuditWindowPayload` (`window_id`-based, `first/last_event_id`, `actor`/`targets`) **cannot be the 0242 payload**: `window_id` is not a substitute for `event_id` (the echo source reads `event_id` only). **`AuditWindowId`/`AuditWindowPayload` are deferred out of this slice** and must be amended to this envelope if/when the wire-contract slice lands (kills the DB-reviewer item-4 "design claims both envelopes" contradiction).

| Field | Window row | Spill row | Pin / rationale |
|---|---|---|---|
| `event_id` | `v_event_id::text` (row PK) | spill key::text (row PK) | **F15 fix** — the receipt echo source (`stub.rs`) and the comparison target (`client.rs`); `window_id` is not a substitute |
| `source_system` | `AUDIT_SOURCE_SYSTEM` = `"aero-im.source"` (new leaf const) | same | **payload-guard gate** (`client.rs` `validate_delivery_payload`); deployment invariant: `AERO_AUDIT_SOURCE_SYSTEM` == const (same convention as 1:1 bindings/harness) |
| `idempotency_key` | `v_event_id::text` (own) | spill key::text (own) | sibling-F1 fix — **own key per row**; the sink must not dedup a spill into its window (the late count is a separate delivery). Never reuse the window key for a different row |
| `count` | N (merged) | 1 | drill arithmetic |
| `aggregated` | `true` | `true` | cross-slice parity-exemption key (sibling §2.4 comment: MUST stay top-level `payload->>'aggregated' = 'true'`; sibling auth slice's `auth_outbox_parity_1to1` uses the same single expression) |
| `spill` | — | `true` | shape discriminator — the parity drill's SUM side cannot otherwise distinguish spill rows from 1:1 rows (both default class `message`) |
| `window_start` / `window_end` | `to_timestamp(floor(epoch/60)*60)` / +60s | same window | window identity (event_id IS the window key for window rows; spill rows carry their window here) |
| `first_event_at` / `last_event_at` | min/max `created_at` timestamps | `NEW.created_at` both | **R8 wins**: sibling deliberately excludes `first/last_event_id` **member refs** ("不带 first/last event_id 引用（避免清扫后悬空引用）") — `audit_events` is retention-swept (`AuditRepo::sweep_before`, legal-hold guarded) + daily partition DROP (0146), while the outbox is never swept (v1-parity durable cursor); member ids dangle post-sweep. The land design's `first/last_event_id` payload refs and its AC2 field list are amended to timestamps |
| `action` | `AGGREGATED_MESSAGE_ACTION` = `"message.batch"` (leaf contract R3 token) | same | single-token arbitration: the land slice re-points to the leaf contract, so the leaf token wins and the sibling's `[PROPOSED] MESSAGE_OUTBOUND_ACTION` `"message.activity"` folds (same single-definition rule as the 0242 file/function arbitration) |
| 0239-family invariants | `event_type`/`schema_id` `'aero.im.security'`, `schema_version` 1, `occurred_at` (window: `v_window_start`; spill: `NEW.created_at`), `class 'message'`, `aggregate_type 'workspace'`, `aggregate_id workspace_id::text`, `data_classification`/`retention_class` | same | leaf `AUDIT_*` consts |
| **Forbidden** | `tenant_id` (payload-guard bail), `actor`/`targets` (sibling §2.4:271 — workspace-level count, not an actor trace), `window_id`, `first/last_event_id` | same | see above |

**Additional §2.1 leaf consts** (join `vocabulary_consts_are_pinned` + truth-check rule 3e allowlist — leaf file + 0242 SQL pin site; the SQL-literal cross-check lives in the **truth-check script**, never a compile-time `include_str!` db_test, which would break the harness's pre-0242 SKIP state):

```rust
pub const LOCAL_ACTION_MESSAGE_CREATE: &str = "message.create";
pub const LOCAL_ACTION_MESSAGE_EDIT:   &str = "message.edit";
pub const AGGREGATED_MESSAGE_ACTION:   &str = "message.batch"; // leaf contract R3 token (sibling "message.activity" folds)
pub const AUDIT_SOURCE_SYSTEM:         &str = "aero-im.source"; // 0242 payload guard value (AERO_AUDIT_SOURCE_SYSTEM invariant)
pub const L1_WINDOW_SECONDS: i64 = 60;
```

## 4. The missing delivery-leg test artifact (settle provable, not asserted)

**Gap (confirmed)**: AC1/T-11 never delivers (closed token endpoint → transport requeue, `terminal == 0` invariant); the parity drill is query-only; AC2 asserts envelope fields, not settle. Nothing in the land design exercises the receipt path for window/spill shapes — a payload missing `event_id`/`source_system` would pass every planned test and kill every window row in production. The sibling's A3 mixed-shape delivery leg was dropped. **Restore and extend it**:

### 4.1 `aero-audit-relay-drill.rs` — mixed-shape delivery leg (the receipt-path proof)

Extension of the existing bin (0239 file gate unchanged; no new b5-pin slot; seed still direct outbox INSERT — this leg proves the *connector* settles the shapes given conforming payloads):

- **Seed set** (split of `rows` = `AERO_AUDIT_DRILL_ROWS`): `rows/3` 1:1-shaped (v4 uuid PK, `payload.event_id` = own PK), `rows/3` window-shaped (md5-derived PK — recomputed in Rust, payload = full arbitrated envelope with `count` 5, `aggregated true`), `rows/3` spill-shaped (own deterministic key, `spill true`, `aggregated true`, `count` 1). **Every conforming payload carries `event_id` == own row PK and `source_system` == config value** (`"aero-im.source"`).
- **Negative controls** (2 rows, seeded with `attempts = 1` so their first claim is attempt 2 → dead in-batch, no sleep needed): ① `payload.event_id` = a different uuid than own PK (correct `source_system`) → must die at **ReceiptMismatch**; ② `payload.source_system` = `"wrong"` (correct `event_id`) → must die at **PayloadGuard**. Both pin that the two gates are live for window-shaped rows and that the permanent class deads after ≤1 retry.
- **Assertions** (existing shape preserved, constants shift): `COUNT(status=2) == rows` (all conforming rows delivered); delivered `event_id` set == seeded conforming set (existing parity); `COUNT(status=3) == 2`; `COUNT(status IN (0,1)) == 0`; `stub.posts() == rows + 2`; optionally `seen_idempotency_keys()` == seeded ids in `AuditId` Display (pins the base32 header spelling for md5 PKs).
- **Why this proves settle**: `status=2` is reachable only through POST → stub echo of `payload.event_id` → `receipt_event_id_matches` vs row PK → `settle`. Any payload missing/mismatching `event_id` or `source_system` on any shape → permanent → dead → the status-2 parity goes red. This is the artifact that makes the §3 pins load-bearing.

### 4.2 `aero-audit-t11-drill.rs` — sink-absent extension (AC1)

Seed window- and spill-shaped rows alongside the 1:1 rows. **Correction**: the extension seeds **must carry `source_system`** (the 1:1 seeds already do) — `validate_delivery_payload` runs before any transport, so a guard-failing window row would dead (permanent) and break the `terminal == 0` invariant. With conforming payloads the invariants hold per-round: `pending == rows`, `terminal == 0`, `SUM(attempts) == rows·round`, `transport_errors == rows` (shapes are claim-shape-agnostic; constants shift automatically since all invariants are written in terms of `rows`).

### 4.3 `aero-audit-l1-parity-drill.rs` — non-vacuous, window-start-scoped, spill-inclusive

Corrections to the §2.5 sketch (acceptance-reviewer gaps (a)/(b) + DB-reviewer item 4):

1. **Self-seed through the trigger** (non-vacuity — today zero production writers exist, so a query-only drill sees `SUM=0 == COUNT=0` and passes even with 0242 dropped): insert ≥1 `message.create` row directly into `audit_events` (fixture shape: `INSERT INTO workspaces`/`participants` + `audit_events`, `audit_governance.rs` `fixture` precedent; `audit_events` DDL admits direct INSERT). The 0242 trigger fires → window rows exist; if 0242 is absent → `SUM=0 ≠ COUNT>0` → **FAIL** (this is the trigger-absence detector).
2. **SUM side**: all `class='message'` rows with `payload->>'aggregated' = 'true'` — window **and** spill (spills carry `count=1`; each audit row contributes exactly 1 to exactly one message-class row). Not "window rows only".
3. **Scope rule on BOTH sides**: per-audit-row window start `floor(extract(epoch FROM created_at)/60)*60 ≥ cutoff` (audit side) and outbox-side `window_start ≥ cutoff` (outbox is never swept, so un-scoped outbox rows would false-red against swept audit rows; window-start scoping on both sides keeps straddling windows consistent; legal-hold carve-outs sit below cutoff on both sides).
4. **Spill leg** (optional but recommended — directly answers "does the FOUND-branch survive delivery"): deliver the window row first (one relay dispatch against the stub, or `SET status=2`), then insert a late same-window audit row → spill row created → deliver it → SUM includes it. Ends with `SUM(count) == COUNT(audit rows in allowlist, window-start-scoped)`.
5. Retention-window exposure documented (same as 0241 precedent, F7).

### 4.4 AC2 additions (`l1_window_aggregates_5_rows_to_1_outbox`)

Envelope field-by-field list gains: `payload->>'event_id' == event_id::text` (row PK — the F15 pin), `payload->>'source_system' == AUDIT_SOURCE_SYSTEM`, `payload->>'idempotency_key' == event_id::text`, `payload->>'aggregated' = 'true'`, `payload->>'first_event_at'/'last_event_at'` present (timestamps — no `first/last_event_id` key, no `window_id` key, no `tenant_id` key: assert absence). Plus the **concurrent two-tx merge db_test** (F5 pin, §2.5 above) and `to_regprocedure('aero_enqueue_l1_aggregate_audit()')` runtime gating (pre-0242 SKIP, sibling AC2 precedent).

## 5. Design-doc deltas applied

1. §2.1 — const block extended (`AGGREGATED_MESSAGE_ACTION`, `AUDIT_SOURCE_SYSTEM`); "Wire envelope per the leaf contract spec" replaced with the arbitrated envelope (§3); `AuditWindowId`/`AuditWindowPayload` deferred with the must-amend note.
2. §2.3 — merge restored to `WHERE status = 0` + immediate `GET DIAGNOSTICS ROW_COUNT` branch (`FOUND` equivalent) before the spill; spill = deterministic key + full payload (own `event_id`/`idempotency_key`, `spill true`, `aggregated true`, `source_system`); merge updates `last_event_at` (timestamp), not `last_event_id`; `search_path = pg_catalog, public`.
3. §2.4 — AC2 envelope assertions per §4.4 (+ concurrent merge db_test).
4. §2.5 — relay-drill delivery leg (§4.1), T-11 `source_system` correction (§4.2), parity-drill corrections (§4.3).
5. §3 — "receipt echo … value-level dual-format — window rows settle" reworded: the *parser* is dual-format; the *echo source* is `payload.event_id`, which the 0242 payload must pin; plus the `source_system` payload-guard gate.
6. §4 — F3 spill row carries own key + markers; F5 gains the READ COMMITTED dependency + 10s lock-wait bound + concurrent db_test; F8 cites the 128-bit digest-space birthday bound (2^122 was the UUID-v4 fresh-key figure — mislabel); F10 canonical search_path; new F11 row: window/spill payload missing `event_id`/`source_system` → `PayloadGuard`/`ReceiptMismatch` permanent → dead after ≤1 retry (F15 + the new gate).
7. §5/§6 — step 6 and the AC table reflect the delivery leg, the parity corrections, and the AC2 additions.
