# Adversarial review — `2026-08-09-land-l1-aggregation-backlog-lanes.design.md`

Scope: the five highest-risk open items against the sibling slice's unresolved findings (`l1-aggregation-in-tx-audit-...-74b1a798`, adversarial stage FAILED: database_reviewer / security_reviewer / test_plan_reviewer / wiring_integrator). All claims verified against the working tree: `pg.rs`, `client.rs`, `stub.rs`, `db.rs`, `audit.rs`, `boot/retention.rs`, 0239/0240/0241 DDL, sibling requirements spec (`docs/requirements/2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.req.md`), leaf wire-contract spec.

## Verdict summary

| # | Item | Verdict |
|---|---|---|
| 1 | `ON CONFLICT DO UPDATE WHERE status IN (0,1)` merge semantics | ❌ **FAIL — regression vs sibling `status = 0`**: merging into a claimed (status=1) in-flight row silently drops the increment when that delivery succeeds. Sibling spec explicitly forbids it |
| 2 | Spill rows with own `idempotency_key` can actually settle (sibling F1) | ❌ **NOT PROVEN as written**: design pins `idempotency_key` (fixes the sink-dedup half of F1) but not `payload.event_id` — the field the receipt echo actually reads. Window rows inherit the leaf envelope which **lacks `event_id` entirely** → echo `"missing"` → permanent dead. No planned test exercises the delivery receipt for either shape |
| 3 | Concurrent-merge serialization | ✅ **Correct under READ COMMITTED as configured** — but the design's F5 row omits the two preconditions the sibling DB reviewer explicitly asked to carry (isolation dependency + statement_timeout lock-wait bound) |
| 4 | Retention-sweep parity | ⚠️ **Direction right, two pins missing**: drill scoping must be window-start-based on both sides; design's `first/last_event_id` payload refs contradict sibling R8 (deliberate exclusion — dangling refs after sweep) |
| 5 | search_path hardening | ✅ **Addressed** — spelling nit (`pg_catalog, public` canonical form); 0239/0241 remain unpinned (sibling follow-up, named) |

---

## 1. `ON CONFLICT DO UPDATE WHERE status IN (0,1)` — FAIL (correctness regression)

**Mechanism (source-verified):** `claim_due` captures the payload snapshot in its `RETURNING outbox.payload` (pg.rs:129-131) and marks status=1; the POST body is that frozen snapshot (`client.rs:201` sends `claim.payload`). `settle` (pg.rs:143-160) is a fenced re-read of **token/status/lease only** — it never re-reads payload. After a successful delivery the row is terminal (status=2) with no re-delivery mechanism; `requeue` (failure path only) is the sole way a merged delta gets delivered.

**The failure:** an event arriving while its window row is claimed (status=1, POST in flight) merges into the row (`status IN (0,1)` matches). The in-flight POST carries count=N; the row becomes count=N+1, delivered. The +1 event is **never delivered to the sink, silently** — and invisible to every in-repo detector, because the parity drill compares the outbox row (N+1) against `audit_events` (N+1): both agree. The sink is the only place with N.

**The sibling spec's own invariant (R3/D5, requirements line 93/139):** `WHERE audit_governance_outbox.status = 0` — "事件永不丢失、永不改写 in-flight/终态行（claim 中改写会破坏 settle 的 fenced 重读）". The DB reviewer verified the `status = 0` form and quantified spill rate ≈ msg_rate × claim→settle RTT (small). The land design's `IN (0,1)` trades that small spill rate for a **silent undercount on the common success path** — the worst outcome class (the sibling security reviewer's F1 was about silent undercount too, at the sink).

**Required change:** merge predicate back to `WHERE audit_governance_outbox.status = 0`; `ROW_COUNT = 0` then uniformly covers status 1/2/3 → spill. The design's "row lock on the PK serializes concurrent writers; no lost updates" (§2.3 step 3) is true for the DB row but false for the sink ledger — the claim as written is misleading.

## 2. Spill rows can settle — NOT PROVEN as written (sibling F1 half-fixed)

**Source-verified receipt contract:** the in-repo stub echoes **`payload.event_id`** (stub.rs:381-390: `payload.get("event_id")`, fallback `"missing"`); `receipt_event_id_matches` (client.rs:539-546) compares the echo value-level against `claim.event_id` (UUID parse first, else base32). The connector **never reads `payload.idempotency_key`** (zero hits outside the stub's observation API). Therefore the load-bearing condition for ANY row shape is:

> `payload.event_id` == row `event_id` (uuid value).

The design pins `idempotency_key` = own event_id (§2.1) — that fixes the *sink-side dedup* half of sibling F1 (a deduping sink no longer collapses spill into window). But it does **not** pin `payload.event_id`:

- **Spill rows:** §2.3 step 4 payload = "`<count 1, own idempotency_key = own event_id>`" — silent on `event_id`. If the implementer keeps the sibling's original shape (`event_id` = window key), the echo mismatches the spill row's own key → `ReceiptMismatch` (permanent) → dead after ≤1 retry. The deterministic spill key `md5(v_key || '|' || NEW.id)` is a good choice (replay-idempotent), but it only settles **if** payload `event_id` = that same key.
- **Window rows:** §2.1 re-points the envelope to the leaf wire-contract spec, whose window envelope has **`window_id`, no `event_id`** (leaf spec field table :123-141; leaf adversarial review F15: "the 19-field window envelope has no `event_id` key"). The stub then echoes `"missing"` → both parse branches fail → `ReceiptMismatch` → permanent → **window row dead after ≤1 retry, never delivered**. The design's §3 claim "receipt echo … value-level dual-format — window rows settle" is only about the *parser*; the *echo source* still requires `event_id` to exist in the payload. As written, **window rows cannot settle under the in-repo contract**.

**No planned artifact catches this:** AC1 (T-11 extension) never delivers (closed token endpoint → transport failure → requeue, `terminal == 0` invariant); the parity drill is query-only; AC2 asserts envelope fields but not `event_id`. The sibling's A3 relay-drill mixed-shape delivery leg (`COUNT(status=2) == N` through the stub — requirements line 120) was the artifact that exercises receipts, and this design **dropped it** (AC4 = T-11 extension + parity drill only).

**Required changes:**
1. Pin `'event_id', <row's own event_id>::text` in BOTH window and spill payloads (0239-mirror shape, as the sibling R4 envelope already does); assert it in AC2's field-by-field list.
2. Restore a mixed-shape **delivery** leg (window-shaped + spill-shaped seeds through the stub → all reach status=2) — without it, a payload missing `event_id` passes every planned test and kills every window row in production.
3. Spill payload needs a `spill: true` marker (sibling D5 shape): the parity drill's SUM side must select `aggregated = true OR spill = true`, and without a marker spill rows are indistinguishable from 1:1-shaped rows (both default class `message`, both md5/uuid event_ids) — the drill would either false-red or silently miss them.

## 3. Concurrent-merge serialization — correct, with two unstated preconditions

**Verified:** the pool sets only `SET statement_timeout = '10000'` (db.rs:31) — no isolation override → server default READ COMMITTED. Under READ COMMITTED, `ON CONFLICT DO UPDATE` re-evaluates both the SET expressions and the WHERE against the winner's *committed* row (EvalPlanQual): queued waiters each re-increment in turn; `(payload->>'count')::int + 1` reads the post-merge count. Within one tx (AC2's 5-row shape) the same tx's own uncommitted row is visible → count accumulates to 5. `ROW_COUNT = 0` ⇔ conflict-with-WHERE-false is exact (no DELETE path on the table). The merge never blocks on a sink round-trip (claim/settle are separate fenced txs).

**What the design's F5 row omits** (both explicitly requested by the sibling DB reviewer, "worth one line in the risk table"):
- The guarantee is **READ COMMITTED-specific**: a future pool move to REPEATABLE READ/SERIALIZABLE turns concurrent same-window merges into 40001 → fail-closed abort of the *message tx* (availability, not correctness).
- The trigger's DO UPDATE can block on the window-row lock held by another message tx; `statement_timeout = 10s` bounds it, but a long-running batch tx on a hot window could push concurrent sends past the timeout → message-tx abort. The design's F2 blast-radius row covers body errors but not lock-wait timeout.

**Recommendation:** add both lines to the failure-mode table, and add a concurrent-merge db_test (two parallel txs, same window, commit both, assert count=2) — the only artifact that pins the EPQ path directly; it also goes red loudly if isolation ever changes.

## 4. Retention-sweep parity — direction right, two pins missing

**Verified sweep topology:** `AuditRepo::sweep_before` (audit.rs:71) hard-deletes `audit_events` older than cutoff (legal-hold guarded), driven by boot/retention.rs `sweep_audit` (`AERO__SERVER__AUDIT_RETENTION_DAYS`), plus daily partition DROP (0146). **No production sweep of `audit_governance_outbox`** (all DELETEs are test-only) — outbox window/spill rows accumulate at 1440/workspace/day for the message lane. This matches v1 (`snaplink_delivery_outbox` also unswept) and the sibling R8 semantic ("aggregate rows are compact durable records, not subject to audit retention") — so the design's F7 "same exposure as 0241, scoped drill" is the right posture. Two things the design must pin:

1. **Drill scoping rule:** "SUM(count) == COUNT(mapped audit)" needs "mapped" defined as per-audit-row **window start** `floor(epoch(created_at)/60)*60 ≥ cutoff` on BOTH sides. Scoping the audit side by `created_at ≥ cutoff` alone produces false divergence for windows straddling the cutoff (surviving rows whose window_start < cutoff are excluded from the SUM side but counted on the audit side). Legal-hold carve-outs don't break the window-start scoping (held rows' windows are < cutoff → excluded on both sides).
2. **Payload member refs contradict sibling R8:** the design's window payload carries `first_event_id`/`last_event_id` member ids; the sibling spec **deliberately excludes** them ("不带 first/last event_id 引用（避免清扫后悬空引用）" — after the sweep they dangle at deleted audit rows; no FK, but the envelope's self-containment is the point, and "compact" is weakened by per-member ids). Since §1's arbitration says exactly ONE 0242 envelope survives, this field set must be negotiated at merge time — the design currently claims both "per the leaf contract spec" (§2.1, window_id-based) and sibling-parity (§3), which are different envelopes. The in-repo contract only requires `event_id` + `idempotency_key` (+ count for the drill); the rest is arbitration.

## 5. search_path hardening — addressed; spelling nit

**Verified:** zero search_path pins repo-wide (security reviewer Q1); single role, all objects in `public`, SECURITY INVOKER everywhere — no live exploit today; shadowing needs CREATE on a path-earlier schema (impossible single-schema). The design's `SET search_path = public` in the 0242 function closes the trigger-body resolution dimension for the new code (the sibling reviewer's recommendation was `SET search_path = pg_catalog, public` — functionally equivalent here since pg_catalog is implicitly searched first, but adopt the canonical spelling to match the review record; it must be a function attribute or first body statement, before the unqualified `audit_governance_outbox` reference resolves). Note the design's own §2.3 correctly keeps 0239/0241 unpinned — that latent exposure is a named sibling follow-up, not this slice's.

## Residuals on the other sibling findings (brief)

- **F2 (allowlist literals unpinned):** closure is feasible and mostly designed — `LOCAL_ACTION_MESSAGE_CREATE/EDIT` consts, `vocabulary_consts_are_pinned` already exists (audit.rs:436-438), truth-check extension mirrors rule 3d mechanics. One implementation hazard: if the "cross-checked by the db_test" plan means `include_str!` of the 0242 SQL, the db_test fails to compile pre-0242 (breaking the harness's SKIP state) — do the literal cross-check in the truth-check script, not in a compile-time test.
- **F4 (trigger-absence detection):** the design's detection is CI-only (parity drill + 0242 file gate). The sibling asked for a production detection gauge (30s sampler, `observability_gauge_samplers` precedent). The design should name this as a residual rather than implying F4 is closed by the drill — a dropped trigger in production is invisible until the next drill run.

## Prioritized remediation

1. **HIGH — payload `event_id` pins** (§2): window payload `event_id = v_event_id`, spill payload `event_id = spill_key`, asserted in AC2 + a restored mixed-shape **delivery** leg through the stub (all shapes reach status=2). As written, every window and spill row dies on receipt; no planned test detects it.
2. **HIGH — merge predicate** (§1): revert to `WHERE status = 0`; `IN (0,1)` silently undercounts at the sink on the delivery-success path, contradicting the sibling spec's explicit invariant.
3. **MED — spill marker + envelope arbitration** (§2.3/§4): `spill: true` in spill payload (parity-drill discriminator); arbitrate the window envelope field set (leaf `window_id` vs sibling 0239-mirror vs member-event-id refs) before writing the SQL.
4. **MED — risk-table lines** (§3): READ COMMITTED dependency + statement_timeout lock-wait bound; add a concurrent two-tx merge db_test.
5. **LOW — nits:** `SET search_path = pg_catalog, public`; parity-drill window-start scoping rule on both sides; truth-check literal cross-check must not be compile-time.
