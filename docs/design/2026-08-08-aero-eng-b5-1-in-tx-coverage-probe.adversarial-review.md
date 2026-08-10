# Adversarial review — `2026-08-08-aero-eng-b5-1-in-tx-coverage-probe.design.md`

Scope: leg C step-6 direct-insert vs 0235/0239 NOT NULL/CHECK constraints · `payload='{}'` cleanup predicate · step-5 `clock_timestamp()+1s` flakiness · F5/F6 relay-off e2e gap · pin-guard sync (:64/:113, 16/21 accounting) · §4 F1–F15 assertion mapping.

**Verdict: leg C's DB-facing mechanics are sound (constraints pass, +1s provably non-flaky, envelope 16-key claim exact, D8 pre-delete exact-match); three real gaps (F5/F6 e2e absent, six F-table rows with no assertion, cleanup predicates should be event_id-scoped), plus two cosmetic sync sites. All amendments below are concrete and costed.**

---

## 1. Step-6 direct insert vs 0235/0239 constraints — PASS, with proof

`INSERT INTO audit_governance_outbox (event_id, payload) VALUES ('…c4','{}'::jsonb)` — column-by-column against `migrations/0239_audit_governance_outbox.sql`:

| column | constraint | verdict |
|---|---|---|
| `event_id` | `UUID PRIMARY KEY` | supplied ✓ |
| `status` | `NOT NULL DEFAULT 0` + `CHECK (status IN (0,1,2,3))` | default ✓ |
| `class` | `NOT NULL DEFAULT 'message'` + `CHECK (class IN ('admin','message','room'))` | default ✓ |
| `priority` | `NOT NULL DEFAULT 10` + `CHECK (priority > 0)` | default ✓ |
| `delivery_mode` | `NOT NULL DEFAULT 'push'` + `CHECK (delivery_mode IN ('push'))` | default ✓ |
| `payload` | `NOT NULL` + `CHECK (jsonb_typeof(payload) = 'object')` | `'{}'::jsonb` → `jsonb_typeof` = `'object'` ✓ **passes** |
| `available_at` / `attempts` / `created_at` | `NOT NULL DEFAULT clock_timestamp()` / `DEFAULT 0` | defaults ✓ |
| `claim_token` / `lease_expires_at` / `delivered_at` / `last_error` | nullable | omitted ✓ |
| `audit_governance_claim_state` CHECK | `(claim_token IS NULL AND lease_expires_at IS NULL) OR (both NOT NULL)` | both NULL → passes ✓ |

**No constraint rejects the row.** The "trigger produces a 16-key envelope" claim is exact: the trigger's `jsonb_build_object` has exactly 16 top-level keys (`event_id, source_system, event_type, schema_id, schema_version, occurred_at, actor, targets, aggregate_type, aggregate_id, action, outcome, payload, data_classification, retention_class, idempotency_key` — 0239 trigger + 0236 v1 same shape). JSONB equality is exact, so a 16-key envelope can never equal `'{}'` — **the cleanup predicate `payload='{}'::jsonb` is unambiguous** (the only `'{}'` row in the leg DB is the direct seed; step-6's cleanup deletes it before any later `'{}'` seed).

Two refinements (not defects):

1. **Scope cleanup by `event_id`, not payload.** Amendments below add three more `'{}'` seeds (steps 6c/6e). Event-id-scoped `DELETE … WHERE event_id='…cX'` is immune to cross-step residue and self-documenting; the payload-uniqueness property is already proven by the check transitions. Keep `payload='{}'` nowhere except optionally a comment.
2. Design step 6 comment says "created_at=now()" — the actual default is `clock_timestamp()` (0239:46), which is *better*: it matches Q8's single clock domain exactly (§4 F15). Cosmetic comment fix only.

## 2. Step-5 `clock_timestamp()+1s` duplicate-seed — NOT flaky (proof), and why

**Non-collision with the step-3 row is provable**: PK is `(id, created_at)` (0146:93). Step 3's row has `created_at = T0` (DEFAULT `now()`); the dup row gets `T1 + 1s` with `T1` = wall clock at step 5. Sequential psql invocations guarantee `T1 > T0`, hence `T1+1s > T0+1s > T0` strictly — a PK collision requires exact equality and is **impossible**, not merely unlikely. The `+1s` is not dead weight either: it makes the dup row's created_at strictly greater than T0 **even in the degenerate case where the two statements somehow shared the same microsecond** (`T1 = T0` ⇒ `T1+1s > T0`) — exactly the boundary the composite PK cares about. Practical case (distinct statement timestamps) would pass without it, but the +1s turns a probabilistic non-collision into a proof — **keep it, and keep the +1s documented as the PK-non-collision guard** (the leg's determinism argument depends on it).

**Partition routing is safe in every midnight case**: 0146 pre-creates today+tomorrow partitions and a `audit_events_default` catch-all that always exists; `now()+1s` can cross at most one UTC midnight, and even a missing daily partition falls into DEFAULT. No "no partition" failure mode exists for this seed.

**Three adjacent step-5 mechanics verified exact** (these are the real flakiness candidates, all deterministic):
- v1 pre-delete (D8): `DELETE FROM snaplink_delivery_outbox WHERE destination='audit' AND idempotency_key='…c2'` — 0236 `aero_enqueue_snaplink_audit` inserts exactly `destination='audit'`, `idempotency_key=NEW.id::text` (0236:100/:121), so the predicate matches exactly one row; without it the v1 `UNIQUE(destination, idempotency_key)` would abort the dup insert (D8 confirmed).
- Dup-row trigger behavior: governance trigger `ON CONFLICT (event_id) DO NOTHING` swallows the second outbox insert (outbox stays 1 row); v1 re-inserts after the pre-delete. Q7 sees `COUNT(a.id)=2 ≠ COUNT(DISTINCT a.id)=1` → `t` → fail-closed. Deterministic.
- Cleanup predicate `detail->>'reason'='leg-c-dup'` matches only the dup row (`'leg-c'` on the step-3 row); no FK/cascade touches outbox rows on audit-row delete (0146: zero inbound FKs).

Residual: the dup row's future `created_at` is invisible to Q1/Q3/Q4/Q6–Q10 (no query filters audit_events by time except Q8's outbox-side window, which this row never enters). No other interaction.

## 3. Relay-off windows F5/F6 — unit-only confirmed; leg C amendments (e2e)

Design §6's unit matrix covers `missing>0 × relay off → None` and `orphans>0 × relay off → Some`, but leg C steps 1–8 never turn the runtime switch off — **the e2e gap is real**. Insertion point: after design step 6's cleanup, before step 7 (step 7's Gate-2 abort test requires runtime ON + binding present; and after step 7 deletes the binding, Q6's enabled-binding join can no longer produce missing>0).

Critical state dependency the amendments must respect: with relay OFF, the existing verdict fails closed on **any** undelivered row ("relay disabled (bindings=N) with X undelivered audit row(s)"). F5's exit-0 contract therefore requires both outbox rows for `…c2` (v1 + 0239) to be marked delivered first — otherwise the relay-health branch preempts the fail-open window.

**Amendment 6b — F5 e2e (fail-open window, exit 0):**

```bash
run_psql ... -v ON_ERROR_STOP=1 -c "
UPDATE snaplink_commercial_runtime SET enabled=FALSE, updated_at=clock_timestamp() WHERE singleton;
UPDATE snaplink_delivery_outbox SET delivered_at=clock_timestamp()
  WHERE destination='audit' AND idempotency_key='…c2';
UPDATE audit_governance_outbox SET status=2, delivered_at=clock_timestamp() WHERE event_id='…c2';
INSERT audit_events (id, workspace_id, action, target, detail)
  VALUES ('…c5','…c1','message.moderated',gen_random_uuid()::text,'{\"reason\":\"leg-c-window\"}'::jsonb);"
```
Gate 1 (runtime off) → trigger `RETURN NEW` → **no outbox row for `…c5`** → missing=1 while the binding stays enabled. Check → **exit 0** + grep `fail-open-window: 1 audit-only row(s)` + grep `verdict: consistent`. (v1/0239 undelivered = 0; orphans = 0; dedup = f.)

**Amendment 6c — F6 e2e (orphan + relay off, exit 1):**

```bash
run_psql ... -c "INSERT INTO audit_governance_outbox (event_id, payload) VALUES ('…c6','{}'::jsonb);"
```
Check → **exit 1** + grep `in-tx-broken` + grep `orphan` (orphan judgment is relay-independent by design). Then `DELETE FROM audit_governance_outbox WHERE event_id='…c6';`.

**Amendment 6d — restore for step 7:** `DELETE FROM audit_events WHERE id='…c5';` + runtime back ON → sanity check **exit 0** + grep `in-tx-ok` + grep `verdict: healthy` (state = audit {c2}, outbox {c2 delivered}, v1 {c2 delivered}, relay on, binding 1). Step 7's zero-escape assertions then hold unchanged (COUNT(outbox event_id='…c2')==1 — the delivered row counts).

**Amendment 6e — F8 e2e (outside-window orphan not false-red):** `INSERT INTO audit_governance_outbox (event_id, payload, created_at) VALUES ('…c7','{}'::jsonb, clock_timestamp() - interval '2 days');` → run check with `AERO__SERVER__AUDIT_RETENTION_DAYS=1` inline → **exit 0** + grep `in-tx-ok` + grep `orphans=0` → cleanup by event_id. (Probe reads the env per process; window=1d excludes the 2-day-old orphan. Robust under any ambient env because the override is inline.)

**Amendment 6f — F1 e2e (coverage-query failure fail-closed):** `ALTER TABLE audit_governance_outbox DROP COLUMN class;` → check → **exit 1** + grep `fail-closed` (Q9 `GROUP BY class` fails under ON_ERROR_STOP; probe aborts before P — deterministic). Step 7's COUNT assertions don't read `class`, so no restore needed on the throwaway DB.

**Amendment 6g — F4 e2e (table absent → no coverage noise):** `DROP TABLE audit_governance_outbox;` → check → **exit 0** + grep `outbox-0239: not migrated` + `! grep 'coverage:'` (Q2 probe → None → coverage skipped; existing verdict drives the exit). Then design step 7's trigger-insert path is unaffected (Gate 2 RAISE fires before any outbox insert; the assertions count audit_events + outbox rows — outbox rows: the failed tx escapes zero rows, so COUNT(outbox …)=0 — **note**: step 7's `COUNT(outbox event_id='…c2')==1` assertion assumes the table still exists; it does — 6g's DROP must therefore run *after* step 7, i.e. order: 6b–6f → design step 7 → 6g → drop DB).

**Amendment 5′ — F13 e2e (--priority preempted by base fail-closed):** inside design step 5's dup-broken state (between the failing check and the cleanup), run `audit-provision-check --priority` → **exit 1** + grep `duplicate audit id` + `! grep 'priority-drill:'` (run_priority's `base.is_error()` early-return is the mechanism; the drill never spawns and D8′ is unreachable).

## 4. Pin-guard sync — verified exact; two cosmetic sites

All design anchors confirmed against source:

| anchor | verified |
|---|---|
| `b5-pin.sh:67` `contract-test-22[PROPOSED]` | ✓ last array entry (index 36) |
| list shape | ✓ python-counted **37 = 15 executed + 22 [PROPOSED]** (regex must slice the array literal — a bare line regex over-counts comment/function lines) |
| guard count check `count -ne 37` | ✓ b5-pin.sh:86-88 |
| `test-b5-pin-guard.sh:64` / `:113` | ✓ both hardcode `"B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"` |
| header comments :6 / :23 | ✓ `15 in-repo-executable names + 22 out-of-repo slots` / `37 slots: 15 executed + 22 [PROPOSED]` |

Accounting sync is automatic after the swap: the guard's PASS line is **dynamic** (`${executed} executed, ${proposed} [PROPOSED]`, b5-pin.sh:130), so swapping `contract-test-22[PROPOSED]` → `audit-governance-coverage` yields `16 executed, 21 [PROPOSED]` with zero guard-code change; the two :64/:113 strings and the :6/:23 comments are the only static sites and the design names them correctly.

**Two cosmetic sync sites the design omits** (comments only, no behavior): `test-b5-pin-guard.sh:39` "Fabricate full verdict evidence for the 15 executed slots" → "16"; b5-pin.sh:4/14 37/37 references are split-agnostic (no change). Also note the fresh-mode evidence requirement automatically extends to the new executed slot — leg C's `b5_check "audit-governance-coverage" "PASS"` (design step 8) and the 0239-absent `SKIP` gate (design step 5) satisfy it; the guard self-test's `positive_log` iterates `ORIG_LIST` dynamically (16 lines, no edit).

## 5. §4 F1–F15 mapping — six rows unmapped; full table after amendments

| F | scenario | assertion mapping (design) | gap → amendment |
|---|---|---|---|
| F1 | Q6–Q10 query failure → exit 1 | parse-level Err units only; no coverage-path e2e | **GAP → 6f** (DROP COLUMN class → exit 1) |
| F2 | `parse_dedup_line` garbage → Err | ✓ unit (design §6) | — |
| F3 | `parse_class_counts` bad lines ignored | ✓ unit (design §6) | — |
| F4 | 0239 absent → silent | ✓ unit (format_report None) + harness SKIP gate | **strengthen → 6g** (DROP TABLE → `not migrated`, no `coverage:` lines) |
| F5 | relay off + missing>0 + orphans=0 → window, exit 0 | ✓ unit matrix (missing × relay off → None) only | **GAP → 6b** (e2e exit 0 + `fail-open-window: 1 audit-only row(s)`) |
| F6 | relay off + missing>0 + orphans>0 → broken | ✓ unit matrix (orphans × relay off → Some) only | **GAP → 6c** (e2e exit 1 + `in-tx-broken` + `orphan`) |
| F7 | windowed orphan → fail-closed | ✓ unit (q8 SQL days=365) + leg C step 6 | — |
| F8 | outside-window orphan → report-only | ✓ unit (q8 SQL days=0 → no clause) | **strengthen → 6e** (2-day orphan + `AERO__SERVER__AUDIT_RETENTION_DAYS=1` → exit 0) |
| F9 | retention env unset/invalid/negative | **no unit test of the env parse at all** | **GAP → U1**: pure `parse_retention_days(raw: Option<&str>) -> i64` (None→365, invalid→365, negative→0, valid passthrough) + thin env wrapper — pure shape avoids env-mutation races in parallel tests |
| F10 | duplicate id → fail-closed | ✓ unit (dedup × on/off → Some) + leg C step 5 | + 5′ (F13 run in dup state) |
| F11 | dead + coverage breach → dead first | no precedence test | **GAP → U5**: snapshot dead=1 + coverage dedup breach → FailClosed reason contains "dead", not "duplicate" (pins the after-dead-branch insertion, §2.2) |
| F12 | binding-less rows not missing | no assertion of Q6's scope join | **GAP → U2**: Q6_SQL text contains `LEFT JOIN snaplink_commercial_bindings b`, `b.workspace_id = a.workspace_id AND b.enabled`, `b.workspace_id IS NOT NULL`, `g.event_id IS NULL` |
| F13 | --priority: base error preempts drill | no assertion | **GAP → 5′** (--priority in dup state → exit 1, no `priority-drill:` output) |
| F14 | multi-binding fan-out impossible | no assertion of the join's keying | **GAP → U2 (fold)**: Q6 join is keyed on `b.workspace_id` (PK — no amplification); assert no `GROUP BY`/`UNNEST` in Q6_SQL |
| F15 | single clock domain | q8 SQL text test (implicit) | **make explicit → U4**: q8_orphan_sql contains `clock_timestamp()`, `LEFT JOIN audit_events a ON a.id = g.event_id`, `WHERE a.id IS NULL`; days=365 → `INTERVAL '365 days'`; days=0 → no `AND` clause |

Plus two unit additions folded into the design's existing list: **U3** Q7_SQL text contains `COUNT(a.id) <> COUNT(DISTINCT a.id)` + `a.action = 'message.moderated'` (pins the audit-side formula that §1.1-A re-grounded), and the design's `format_report`-with-`coverage: None` test (F4 unit half). Post-amendment: **all 15 rows map to ≥1 unit or integration assertion.**

## 6. Costed amendments

| # | file | delta |
|---|---|---|
| 6b/6c/6d | `scripts/test-integration.sh` leg C | +~28 lines (F5/F6/restore, seeds `…c5`/`…c6`) |
| 6e | leg C | +5 lines (`…c7`, inline retention env) |
| 6f | leg C | +4 lines (DROP COLUMN class → exit 1) |
| 6g | leg C (after step 7) | +4 lines (DROP TABLE → `not migrated`) |
| 5′ | leg C step 5 | +3 lines (--priority run in dup state) |
| U1–U5 | `tests/audit_provision.rs` | +~45 lines (env-parse pure fn, 3 SQL-text pins, dead-vs-coverage precedence) |
| `audit_coverage.rs` | §2.1 API | +1 pure fn `parse_retention_days` (keeps ≤160 target; probe's env read delegates to it) |
| pin | `test-b5-pin-guard.sh:39` | 1 comment word (cosmetic) |
| pin | design §7 spec | cleanup predicates → `WHERE event_id='…cX'` (3 sites); step-6 comment "now()" → `clock_timestamp()` default |

No DDL, no CLI, no `audit_provision.rs` delta change beyond the design's ≤13 lines. All new assertions are deterministic under the existing throwaway-DB discipline (fresh DB per leg; fixed UUID seeds `…c0`–`…c7`; `AUDIT_PROVISION_COVERAGE_DB` + `assert_disposable_db_name` entry per design §5).
