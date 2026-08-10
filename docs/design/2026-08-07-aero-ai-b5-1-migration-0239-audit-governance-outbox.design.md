# Design: Land migration 0239 — `audit_governance_outbox` DDL + additive parallel trigger

> Status: proposed · Slices: B5-1 (migration + trigger + `governance.rs` commit) · Depends on: 0235/0236 (already landed) · Does **not** depend on: B5-2/B5-3/B5-4 (their seams are already in-tree, file-gated on this migration)
> Requirements: `docs/requirements/2026-08-07-aero-ai-b5-1-migration-0239-governance-outbox.req.md`
> This direction: **the single blocker** — every B5 acceptance check (`a3-relay-drill`, `t11-fail-closed`, `moderation-priority-drill`, `audit-provision-check`, `audit_governance::`/`moderation_finalize_outbox_parity` entries) is file-gated on `migrations/0239_audit_governance_outbox.sql` and currently reports SKIP.

## 0. Evidence verification (untrusted claims → confirmed repo facts)

All direction citations were re-checked against the working tree on 2026-08-07.

| Cited claim | Verification result |
|---|---|
| `migrations/` ends at 0238, no 0239 ever | ✅ `ls migrations/*.sql \| wc -l` = 238; last = `0237_snaplink_destination_credentials.sql` + `0238_message_recall.sql`; `git log` shows no 0239 |
| `governance.rs` anchors with drift | ✅ **untracked** (`?? crates/aero-ai/src/governance.rs`); `GOVERNANCE_PRIORITY_MODERATION: i16 = 100` at :31, `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` at :33 (doc: "also the 0239 column default"), `GovernanceLane` at :61, `governance_lane_for` at :86. `LOCAL_ACTION_MODERATED = "message.moderated"` at :49, `MODERATION_OUTBOUND_ACTION = "admin.content.flag"` at :60 |
| 4 file-gates for `0239_audit_governance_outbox.sql` in `scripts/test-integration.sh` | ✅ gates at :306, :325, :351, :447 (evidence cited 306/325/347/447 — drift, count holds). Each `[ -f migrations/0239_audit_governance_outbox.sql ]` branch runs the section, else emits `b5_check "<name>" "SKIP (0239 not landed)"` |
| Drill SKIP probes | ✅ all three drills probe `SELECT to_regclass('audit_governance_outbox')::text`, absent → stderr SKIP line + `exit(2)`: t11-drill:73-83, relay-drill:57-67, priority-drill:87-118. Priority drill additionally probes `priority`/`class` columns via `information_schema.columns` (:113-118) — column absence → SKIP exit 2; ordering absence → **FAIL** (assertions at :208-231, :288-310). t11/relay drills INSERT without `priority`/`class` (t11:127-132, relay:98-103) ⇒ **both columns need defaults** |
| `pg.rs` claim_due | ✅ `claim_due` :69, `settle` :110, `requeue` :155, `mark_dead` :189 (`crates/aero-audit-connector/src/pg.rs`); claim SQL (:78-102) has **no priority term** — `ORDER BY available_at, created_at, event_id` (B5-3's scope); status constants 0/1/2/3 at :27-30 |
| `worker/mod.rs:401` + `messages.rs:632` | ✅ `Some(LOCAL_ACTION_MODERATED)` at `crates/aero-ai/src/worker/mod.rs:401`; `workspace.map(\|_\| "message.moderated")` at `crates/aero-im-core/src/service/messages.rs:636` (evidence said :632 — drift). `message_reports.rs:305` third producer |
| 0236 trigger precedent | ✅ `audit_events_snaplink_delivery` AFTER INSERT at 0236:129-133; fn `aero_enqueue_snaplink_audit` :68; runtime gate (:75-79) + binding lookup `aero_snaplink_binding_for_workspace` (:84, RAISE at 0235:206) + `action` passed through verbatim |
| Relay boot degrade (F13) | ✅ `crates/aero-server/src/bin/main.rs:245-266` — presence-gated on `AERO_AUDIT_TOKEN_ENDPOINT`; comment verbatim: "booting before the 0239 table lands degrades to logged claim errors, not a crash" |

**Corrections & contract deviations** (adopted from the requirements spec §1.1 and R1/R2 vs campaign A1-C; all confirmed against the working tree):

| # | Contract citation | Contract text | This design | Resolution |
|---|---|---|---|---|
| C1 | req §1.1 | governance.rs anchors at :32,58-69,84 | actual :31/:33/:61-74/:71/:86-94 | ✅ **adopted** — drift confirmed (file untracked; anchors re-verified) |
| C2 | req §1.1 | drill lane constants 100/200 = this direction's | they belong to B5-3's reconciliation, not this direction's 10/100 | ✅ **adopted** — 0239 pins only the column default 10; 100/200 vs 10/100 is B5-3's reconcile (§7 risk) |
| C3 | req §1.1 | trigger = enqueue redirect (改写 0236) | **additive parallel trigger** — 0236 trigger untouched, not rewritten | ✅ **adopted** — additive verified against 0236:129-133; A2 halves (3)+(4) hold under either firing order |
| C4 | req §1.1 | in-tx mechanism (unpinned) | AFTER INSERT **row** trigger; **zero Rust production changes** | ✅ **adopted** — producer call shapes frozen (worker/mod.rs:401, messages.rs:636, message_reports.rs:305) |
| C5 | req R2 | `CREATE FUNCTION aero_enqueue_governance_audit()` + `CREATE TRIGGER audit_events_governance_enqueue` | initial draft: `aero_enqueue_audit_governance()` + `audit_events_governance` (spelling carried from the 08-06 campaign design's redirect-target prose) | ✅ **aligned to R2** — R2 is the shared contract for both B5-1 slices (req §7: "§4 R1/R2 即共享 DDL 契约"); zero in-repo consumers grep either spelling (verified); firing order preserved (`audit_events_governance_enqueue` < `audit_events_snaplink_delivery`, 'g' < 's'); the 08-06 campaign-design spelling is superseded — the sibling's redirect must target R2's name |
| C6 | req R1 | `priority` type = integer | `priority SMALLINT NOT NULL DEFAULT 10` | ⚠️ **documented deviation, non-blocking** — SMALLINT = i16 = the fixture-authority type (`governance.rs` `GOVERNANCE_PRIORITY_*: i16`), the 0130 ai_jobs precedent (`priority SMALLINT NOT NULL DEFAULT 100`), and the B5-3 doc's shape; the priority drill binds i64 literals (100/200) — PG server-side coercion, fits; every consumer works either way (§4 contract note) |
| C7 | req R2 vs campaign A1-C(3)/(4) | R2 payload-envelope list: `action` = NEW.action 原样本地 token + flat `workspace_id`/`actor_id`/`target`/`detail`/`created_at` keys | v2 envelope: outbound `action` = `'admin.content.flag'` (`MODERATION_OUTBOUND_ACTION`) + `actor`/`targets`/`aggregate_id`/`payload` shape (§5) | ⚠️ **supersession documented** — R2's list is a stale mirror of the 0236 **v1** envelope; A1-C(3) pins the governance row's outbound `action='admin.content.flag'` and A1-C(4) pins the local token **on the v1 row only**; this design follows A1-C(3)/(4) — the local token never appears on v2 (A2 field assertions) |
| C8 | req R1 (consumer-pinned column union) | R1 enumerates 12 consumer-pinned columns | + `delivery_mode TEXT NOT NULL DEFAULT 'push'` (13th, defaulted, zero consumers) | ✅ **additive, documented** — promised shape from the campaign design + proposals anchor (08-06 campaign design §8 B5-1 / audit-contract-batch §B5-1); can never break a drill (every drill INSERT names its columns); §4 contract note |

## 1. Current state (the only missing link)

Everything downstream already exists and is green-or-SKIP:

```
audit_events (partitioned, 0146) ──AFTER INSERT──▶ [0236] aero_enqueue_snaplink_audit → snaplink_delivery_outbox (v1)
                                                      │
                                                      └─▶ [MISSING 0239] audit_governance_outbox (v2) + trigger
                                                              ▲
                                              PgOutboxRepo (B5-2, in-tree) claims/settles/requeues/deads it
                                              aero-eng::audit_provision (B5-4) buckets it (Q3: status 0/1/2/3)
                                              aero-ai::governance (untracked) is the mapping fixture authority
```

`governance.rs` is fully wired (`worker/mod.rs:58` imports it; `cargo test -p aero-ai --lib` = 205 passed including its unit tests). The file just needs committing. The connector crate (`crates/aero-audit-connector`, untracked) compiles and its suite is green (see §6 A4).

## 2. Scope

**In scope (this slice):**
1. `migrations/0239_audit_governance_outbox.sql` — table DDL + additive parallel trigger (R1 + R2 contracts below).
2. Commit `crates/aero-ai/src/governance.rs` (already wired; the fixture authority the trigger duplicates as SQL literals with cross-slice pin comments).
3. No Rust production-code changes to `worker/mod.rs`, `messages.rs`, `pg.rs`, `main.rs` — but two landing-time seam reconciliations land here: the `network relay-probe` arm in `crates/aero-cli/src/main.rs` (B5-2's harness gate consumer, handoff H2) and the `audit_governance` contract-fixture module + db_tests in aero-storage (H3).

**Out of scope (owned by siblings, must NOT land here):** `AuditGovernanceOutboxRepo` in aero-storage (the sibling adds it to the same `audit_governance` module — handoff H3); B5-3 `ORDER BY priority DESC` claim change; B5-4 provisioning; v1/v2 coexistence reconciliation (the 0236 v1 trigger stays live verbatim; the redirect needs its own `ON CONFLICT (event_id) DO NOTHING` — handoff H4). The `audit_governance::`/`moderation_finalize_outbox_parity` **db_tests are landed in this batch** — they were the same-batch obligation: landing the migration flips the harness entries from SKIP to run, and the empty-filter guard fails red otherwise (H3).

## 3. API changes

**SQL surface only — zero Rust API changes.** New database objects:

| Object | Kind | Consumer seam |
|---|---|---|
| `audit_governance_outbox` | table | `PgOutboxRepo` (claim/settle/requeue/mark_dead SQL), 3 drills, `aero-eng::audit_provision` Q3-Q5 |
| `aero_enqueue_governance_audit()` | plpgsql function (AFTER INSERT row trigger body) | fires on every `audit_events` INSERT in the same tx |
| `audit_events_governance_enqueue` | trigger on `audit_events` | additive — `audit_events_snaplink_delivery` (0236) untouched |

(R2 names, corrections C5.)

The trigger is the **in-tx enqueue** (R3): soft-delete + audit append + governance row commit or roll back together. `AuditRepo::append_in_tx` (aero-storage events.rs:337) and the moderation finalize seams (`worker/mod.rs:401`, `messages.rs:636`, `message_reports.rs:305`) are unchanged; they already pass `Some("message.moderated")`, which is all the trigger keys on.

## 4. R1 — DDL contract (12 consumer-pinned columns)

```sql
-- Migration 0239: audit governance outbox (B5-1).
-- v2 outbox for the audit relay: 1:1 with audit_events.id (event_id =
-- audit_events.id). Status machine 0=enqueued 1=claimed 2=delivered 3=dead
-- (pin: crates/aero-audit-connector/src/pg.rs STATUS_ENQUEUED..STATUS_DEAD).
-- Additive: the 0235/0236 v1 path (snaplink_delivery_outbox) is untouched.

CREATE TABLE IF NOT EXISTS audit_governance_outbox (
    event_id          UUID        PRIMARY KEY,        -- = audit_events.id (1:1, A2 join key; satisfies the pinned "UNIQUE(event_id)" dedup contract)
    status            INTEGER     NOT NULL DEFAULT 0
                      CHECK (status IN (0, 1, 2, 3)), -- connector status machine (pg.rs:27-30)
    class             TEXT        NOT NULL DEFAULT 'message', -- 'admin'|'message'|'room' (governance.rs GOVERNANCE_CLASS_*); t11/relay drills omit it → default required
    priority          SMALLINT    NOT NULL DEFAULT 10, -- DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG (governance.rs:33); t11/relay drills omit it → default required; DESC lane: higher = claimed first (B5-3)
    delivery_mode     TEXT        NOT NULL DEFAULT 'push', -- reserved (design-doc promised shape; no consumer yet — B5-3 delivery policy)
    payload           JSONB       NOT NULL
                      CHECK (jsonb_typeof(payload) = 'object'), -- relay forwards verbatim; sink Idempotency-Key = event_id
    available_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), -- claim filter (pg.rs:82); single clock domain = clock_timestamp()
    attempts          BIGINT      NOT NULL DEFAULT 0 CHECK (attempts >= 0), -- post-increment per claim (pg.rs:97)
    claim_token       UUID,       -- rotated per claim (pg.rs:94)
    lease_expires_at  TIMESTAMPTZ,
    delivered_at      TIMESTAMPTZ, -- settle stamp (pg.rs:134)
    last_error        TEXT,       -- requeue/dead detail (pg.rs:137,150)
    created_at        TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), -- claim ORDER BY tiebreak (pg.rs:87)
    CONSTRAINT audit_governance_claim_state CHECK (
        (claim_token IS NULL AND lease_expires_at IS NULL)
        OR (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    ) -- mirror v1 0235:178-181
);

-- Due index: claim filter + ORDER BY (pg.rs:78-88) with status IN (0,1).
-- B5-3 extends ORDER BY with priority; the index change is B5-3's, not this slice's.
CREATE INDEX IF NOT EXISTS audit_governance_due_idx
    ON audit_governance_outbox (available_at, created_at, event_id)
    WHERE status IN (0, 1);
```

Contract notes:
- `event_id` is the PK. It satisfies the pinned "UNIQUE(event_id)" dedup promise (drills' `ON CONFLICT`/parity oracles key on it) — `PRIMARY KEY` and `UNIQUE` are equivalent here; PK chosen because there is no other natural key and `ON CONFLICT (event_id)` is used by the trigger.
- **No FK to `audit_events` — deliberate; the 1:1 join invariant is creation-time-only (explicit decision).** `event_id` is a bare UUID PK, **not** `REFERENCES audit_events(id)`. The retention sweep (`AERO__SERVER__AUDIT_RETENTION_DAYS`, boot/retention.rs `sweep_audit` row-DELETE + `sweep_audit_partitions` whole-partition DROP, :301/:325) purges `audit_events` with no outbox check and no FK to stop it, so a pending/claimed outbox row **can** outlive its source event (e.g., a requeue loop or a long relay outage spanning the retention window). The design therefore states the invariant as **"outbox row ⟹ source row existed at commit"** — guaranteed airtight by the in-tx AFTER INSERT trigger (MVCC: the pair commits/rolls back atomically, never partially observable) — **not** "⟹ source row still exists". Consequences, stated now so they are not surprises at parity-test time: (a) delivery fidelity is unaffected — the payload is a self-contained sanitized snapshot (v1's `aero_snaplink_audit_payload` philosophy, 0236), so the relay still receives a faithful event after its source row is swept; (b) any join-based accounting (A2's 1:1 join, `aero-eng::audit_provision` counts) must treat the `audit_events` side as best-effort for rows older than the retention window — pending rows whose source is gone are legal, not corruption; (c) this mirrors v1's identical exposure (its `aero_reconcile_snaplink_audit` is equally blind to swept rows), so it is precedent-consistent, not a regression; (d) an FK would be actively wrong — it would make row DELETE / partition DROP fail or cascade into the outbox, i.e., retention would be hostage to the relay's delivery progress.
- 12 consumer-pinned columns = `event_id, status, class, priority, payload, available_at, attempts, claim_token, lease_expires_at, delivered_at, last_error, created_at`. `delivery_mode` is a 13th, defaulted, zero consumers — include it (design-doc promised shape) or drop it; it can never break a drill because every drill INSERT names its columns explicitly.
- `status` is `INTEGER` (i32, matching the connector's i32 constants) not SMALLINT.
- `priority` is `SMALLINT` (i16, matching `governance.rs` constants) with `DEFAULT 10` — required because t11/relay drills seed without it. The priority drill binds i64 literals (100/200) into it — fine for SMALLINT. R1 says `integer`; SMALLINT is the fixture-authority type and every consumer coerces — documented deviation, corrections C6.
- `class` default `'message'` — required by the same omission.
- `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` + `DROP TRIGGER IF EXISTS` before `CREATE TRIGGER` — the house style for idempotent migrations (0236 precedent).

## 5. R2 — additive parallel trigger contract

```sql
-- Token-keyed enqueue (R2): ONLY audit action 'message.moderated' enters the
-- governance outbox. Every other action passes through untouched (fail-open;
-- never raise, never block) — they keep flowing through the 0236 v1 trigger.
CREATE OR REPLACE FUNCTION aero_enqueue_governance_audit()  -- R2 name (corrections C5)
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enforcement_enabled BOOLEAN;
    binding snaplink_commercial_bindings%ROWTYPE;
BEGIN
    -- Gate 1 (fail-open, runtime): mirror 0236:75-79. A2 half (4)
    -- moderation_finalize_runtime_disabled_commits_1_plus_0 pins |G|=0 when
    -- disabled — dropping this check breaks that half.
    SELECT runtime.enabled INTO enforcement_enabled
      FROM snaplink_commercial_runtime runtime
     WHERE runtime.singleton;
    IF NOT COALESCE(enforcement_enabled, FALSE) THEN
        RETURN NEW;
    END IF;

    -- Token-keyed mapping (fail-open pass-through): only the moderation token
    -- maps; unknown tokens must NOT raise (a second abort path would block all
    -- room.*/message.* audit rows through the shared trigger — design R-D2).
    IF NEW.action <> 'message.moderated' THEN
        RETURN NEW; -- cross-slice pin: governance_lane_for(other) == None
    END IF;

    -- Gate 2 (fail-closed, binding): mirror 0236:84. RAISE aborts the whole
    -- tx (soft delete + audit + outbox together). A2 half (3)
    -- moderation_finalize_without_binding_aborts_tx pins this; keeping the
    -- lookup in THIS trigger makes the abort independent of trigger order
    -- (audit_events_governance_enqueue fires before audit_events_snaplink_delivery:
    -- 'g' < 's') and of the sibling's later redirect.
    binding := aero_snaplink_binding_for_workspace(NEW.workspace_id);

    -- cross-slice pin: must equal aero_ai::governance::governance_lane_for(
    --   "message.moderated") → GovernanceLane { class: "admin",
    --   priority: 100 /* GOVERNANCE_PRIORITY_MODERATION */,
    --   outbound_action: "admin.content.flag" /* MODERATION_OUTBOUND_ACTION */,
    --   status: 0 }
    INSERT INTO audit_governance_outbox
           (event_id, status, class, priority, payload)
    VALUES (
        NEW.id,
        0,
        'admin',
        100,
        jsonb_build_object(
            'event_id', NEW.id::text,
            'source_system', binding.source_system,
            'event_type', 'aero.im.security',
            'schema_id', 'aero.im.security',
            'schema_version', 1,
            'occurred_at', NEW.created_at,
            'actor', jsonb_build_object(
                'id', COALESCE(NEW.actor_id::text, 'system'),
                'type', CASE WHEN NEW.actor_id IS NULL THEN 'system' ELSE 'participant' END
            ),
            'targets', CASE WHEN NEW.target IS NULL THEN '[]'::jsonb
                            ELSE jsonb_build_array(jsonb_build_object(
                                'id', NEW.target, 'type', 'resource')) END,
            'aggregate_type', 'workspace',
            'aggregate_id', NEW.workspace_id::text,
            'action', 'admin.content.flag',  -- MODERATION_OUTBOUND_ACTION (A2 field assertion; supersedes R2's "local token on v2" envelope — A1-C(3), corrections C7)
            'outcome', 'success',
            'payload', aero_snaplink_audit_payload(NEW.detail),
            'data_classification', 'confidential',
            'retention_class', 'security',
            'idempotency_key', NEW.id::text
        )
    )
    ON CONFLICT (event_id) DO NOTHING; -- idempotent vs sibling redirect / replay
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS audit_events_governance_enqueue ON audit_events;  -- R2 name (corrections C5)
CREATE TRIGGER audit_events_governance_enqueue
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_governance_audit();
```

R2 properties, each with its pin:
- **Token-keyed** — keyed on `NEW.action` only, never caller identity (`message_reports::review_authorized` passes `actor=Some(reviewer)` and must stamp identically; A2 half 7).
- **Pass-through no-raise** — single-token match; any other action returns `NEW` before any INSERT. A raise here would be a second abort path (E4's binding RAISE is the fail-closed path; the mapping must stay fail-open). Pinned by governance.rs units `unknown_local_token_passes_through_unmapped` + `user_delete_token_stays_out_of_admin_lane` and A2 half 9.
- **Cross-slice pin comments** — every literal carries a `-- cross-slice pin:` comment naming the `aero_ai::governance` constant. aero-storage production code must never import aero-ai (dependency direction), so textual pins + behavioral drills (A3) are the drift guards.
- **Envelope supersession (R2 → A1-C)** — R2's payload-envelope list (`action` = local token verbatim + flat `workspace_id`/`actor_id`/`target`/`detail`/`created_at` keys) is a stale mirror of the 0236 **v1** envelope. A1-C(3) pins the governance row's outbound `action` = `'admin.content.flag'`; A1-C(4) pins the local token on the **v1** row only. This design follows A1-C(3)/(4): v2 `action` = `MODERATION_OUTBOUND_ACTION` and the local token never appears on v2; the sanitized detail reaches v2 under `payload` via `aero_snaplink_audit_payload(NEW.detail)`, and identity is carried by `event_id`/`idempotency_key` (corrections C7).
- **Gate semantics preserved** — runtime gate (fail-open skip) + binding lookup/RAISE (fail-closed abort) both replicated. 0236's own gate stays untouched, so a moderation row can never reach v2 (a) while disabled, or (b) without a binding. Any gate drop fails A2 halves (3)+(4).
- **Disabled-window asymmetry — ~~accepted by design, permanently out-of-band for v2~~ SUPERSEDED (2026-08-08): the re-open condition below is triggered by the security review of the verified landing; migration 0241 `aero_reconcile_governance_audit()` + relay-tick wiring close it (see the landing design doc §7.1).** While the runtime kill-switch is `enabled=FALSE`, a `message.moderated` audit row produces **no v2 outbox row at trigger time**: an AFTER INSERT trigger never re-fires for that row. v1's `aero_reconcile_snaplink_audit` (0236:230+) incidentally recovers v1 rows accepted while the switch was off, and its dispatch loop runs it ahead of every claim batch — v2 had no equivalent, so a disabled-window moderation was permanently absent from the external governance lane. **Original decision: the v2 governance lane gets no equivalent in this campaign.** Rationale: (a) nothing is lost — the audit row itself commits in the same tx as the soft delete and is untouched by the gate; only relay delivery is skipped, which is the designed meaning of the kill-switch (fail-open polarity, opposite axis of the fail-closed binding RAISE); (b) the payload is a deterministic function of the audit row, so catch-up delivery is *always* reconstructible later; (c) a runtime backfill would make |G| time-dependent and contradict the pinned fail-open semantics of A2 half 4 (`moderation_finalize_runtime_disabled_commits_1_plus_0` asserts |G|=0 while disabled). **Re-open condition (recorded for the sibling slice)**: if the governance relay contract ever requires catch-up delivery for rows accepted while disabled, add that backfill slice then — it does not belong in this migration. **2026-08-08 resolution**: the security review of the landing triggered the re-open condition (if the external sink is contractually the authoritative moderation record, a switch toggle must not permanently gap the lane). 0241 mirrors 0236 with three v2 adaptations: token-keyed scan (`action = 'message.moderated'` only — never fabricates `admin.content.flag` for unmapped actions, R-D2), `event_id`-keyed NOT EXISTS + `ON CONFLICT (event_id) DO NOTHING`, INSERT byte-identical to this trigger's INSERT; runtime-gate-free by design (the recovery path exists because the gate skipped enqueue — v1 parity). `Relay::dispatch_batch` calls `reconcile` before `claim_due` on every tick (v1 `runtime.rs:215-216` ordering), so the disabled window self-heals on the first tick after re-enable; the A2-half-4 |G|=0 contract is unchanged (it pins the *trigger-time* behavior; reconcile is a separate, documented recovery path). Oracle: `governance_reconcile_backfills_disabled_window` db_test.
- **In-tx** — AFTER INSERT row trigger fires inside the audit append transaction; abort ⇒ soft delete + audit + governance row all roll back together (R3). Zero Rust changes.
- **Additive** — 0236's `audit_events_snaplink_delivery` trigger is not modified, dropped, or replaced; v1 rows keep flowing (v1/v2 coexistence reconciliation is the sibling slice's).

## 6. Compatibility constraints

1. **Partitioned-parent trigger**: `audit_events` is RANGE-partitioned (0146). Row-level AFTER triggers defined on the parent fire for partition inserts — proven in production by the 0236 trigger on the same table. No partition-local trigger needed.
2. **Trigger firing order**: `audit_events_governance_enqueue` < `audit_events_snaplink_delivery` alphabetically ('g' < 's') ⇒ governance fires first. The design is order-independent (gates + rollback make both orders behaviorally identical), but the order note is recorded for the sibling's redirect review.
3. **Drill INSERT shapes**: t11/relay drills insert `(event_id, payload, available_at, attempts, status)` only ⇒ `class`/`priority`/`delivery_mode` must all have defaults. Priority drill inserts `priority`/`class` explicitly (100/200 — B5-3's reconciliation values, not this slice's 10/100).
4. **`ON CONFLICT (event_id) DO NOTHING`** keeps the trigger idempotent when the sibling's `CREATE OR REPLACE` redirect of the enqueue path later lands — a plain INSERT would raise a duplicate-key and abort unrelated audit transactions.
5. **`clock_timestamp()` everywhere** — DEFAULTs and trigger use `clock_timestamp()` (not `now()`/`transaction_timestamp()`) to stay on the relay's single clock domain (pg.rs module doc); the A2/A3 fixture's `available_at` backdating is an UPDATE on seam-produced rows, not a bespoke INSERT (design R6).
6. **`status IN (0,1)` is the claimable window** (pg.rs:81) — the DDL CHECK must never be widened to include a non-terminal `4` without a connector change; the partial due index must keep `WHERE status IN (0,1)`.
7. **No dependency on post-0239 objects** — `make migrate-smoke` globs `migrations/*.sql` in order; 0239 references only 0235/0236 objects (`snaplink_commercial_runtime`, `snaplink_commercial_bindings`, `aero_snaplink_binding_for_workspace`, `aero_snaplink_audit_payload`).
8. **Compile-time embedded migrations** — 0239 is baked into the `aero-cli` binary by `sqlx::migrate!("../../migrations")`. Building before migrating is mandatory (AGENTS §4.2); running `aero-cli migrate` on a stale binary silently no-ops the new file.
9. **Sibling batch ordering (reconciled at landing, H3)** — the harness `audit_governance::`/`moderation_finalize_outbox_parity` entries flip from SKIP to RUN the moment this file lands; their db_tests landed with this batch (`crates/aero-storage/src/audit_governance.rs`, registered in lib.rs), so the empty-filter guard (`test result: ok. [1-9][0-9]* passed` required — no vacuous green) cannot go red. Priority drill FAILS red (not SKIP) until B5-3's `ORDER BY priority DESC` lands — the honest G6 window is documented, not suppressed.
10. **Retention sweep vs. outbox — creation-time-only invariant (see §4 note)** — `sweep_audit`/`sweep_audit_partitions` (boot/retention.rs:301/:325, gated by `AERO__SERVER__AUDIT_RETENTION_DAYS`) purge `audit_events` rows and DROP daily partitions with no outbox check and no FK to stop them. Pending/claimed outbox rows may therefore outlive their source event; relay delivery stays valid (self-contained snapshot payload), and any `audit_events` join is valid only for rows inside the retention window. The DDL must keep `event_id` FK-free — an FK would make partition DROP / row DELETE fail or cascade, hostage to relay delivery progress.

## 7. Failure modes

| # | Failure | Detection / mitigation |
|---|---|---|
| F1 | Stale binary migrate (0239 silently no-op) | `cargo build` → `aero-cli migrate` order (AGENTS §4.2); A1.1 `make migrate-smoke` replays the full chain on a fresh DB; drills exit 2 with the SKIP verdict line until the table exists |
| F2 | `priority`/`class` defaults missing | t11/relay drills fail at seed INSERT (NOT NULL violation) → section FAILS red |
| F3 | Mapping raises on unmapped tokens | every non-moderation audit tx aborts (delete + audit + outbox). Pinned by `unknown_local_token_passes_through_unmapped` unit + A2 half 9; the trigger has no ELSE branch |
| F4 | Runtime gate dropped | `moderation_finalize_runtime_disabled_commits_1_plus_0` (A2 half 4) fails: |G|=1 expected 0 |
| F5 | Binding gate dropped | moderation rows enqueue for unprovisioned workspaces (fail-open downgrade); A2 half 3's abort path stops exercising; sibling's mechanical gate weakened. Mitigation: this trigger keeps its own `aero_snaplink_binding_for_workspace` lookup |
| F6 | Double-enqueue (sibling redirect lands, both paths INSERT) | `ON CONFLICT (event_id) DO NOTHING` dedups; without it, `UNIQUE` violation aborts unrelated txs (see A2 half 1 oracle: "|G|=2 or UNIQUE violation") |
| F7 | Status CHECK widened / claim window drift | connector fences (`status IN (0,1)`) and `audit_provision` Q3 buckets diverge; provision-check leg C/D verdicts fail |
| F8 | Index shape drift | claim ORDER BY `(available_at, created_at, event_id)` without matching index → seq scan at scale (no correctness failure; perf only) |
| F9 | Trigger never fires (typo in trigger name/function) | A2 half 1 `\|G\|=0` fails; relay drills seed directly so they cannot catch it — the parity db_test is the only in-tx oracle, hence the non-vacuous harness guard |
| F10 | `delivery_mode` CHECK too strict (future values) | no consumer yet — keep it unconstrained TEXT with default; B5-3 owns any CHECK |
| F11 | Harness runs the drill section before 0239 in same commit | `set -euo pipefail` + `[ -f ]` gate: drill exit 2 would fail the section red — the migration file and its consumers must land atomically in one commit |
| F12 | Retention sweep orphans a pending/claimed outbox row (source `audit_events` row hard-DELETEd or partition DROPPed past `AERO__SERVER__AUDIT_RETENTION_DAYS`; no outbox check, no FK) | **Accepted by design** (§4 note, §6.10): delivery fidelity preserved — payload is a self-contained snapshot; the 1:1 join invariant is creation-time-only. Legal-hold guard in `sweep_before`/`ensure_audit_event_partitions` already protects held workspaces. Precedent-consistent with v1, not a regression |

## 8. Migration steps (ordering is load-bearing)

1. `cargo build` (embeds the new migration into `aero-cli`; **never** `aero-cli migrate` on a stale binary — F1).
2. Fresh throwaway DB: `make migrate-smoke` (replays 0001→0239 chain, `ls migrations/*.sql | wc -l` = 239 after this lands — never hardcode).
3. Sanity-probe the DDL shape on the throwaway DB: `\d audit_governance_outbox` + insert a probe `audit_events` row with `action='message.moderated'` via a `snaplink_commercial_runtime.enabled=TRUE` + enabled binding workspace and confirm 1 governance row (class='admin', priority=100, status=0); then a non-moderation action and confirm 0 rows.
4. Run the B5 section of `scripts/test-integration.sh` (or the full script) — all previously-SKIP `b5_check` slots must flip to PASS (`a3-relay-drill`, `t11-fail-closed`, `moderation-priority-drill`, `audit-provision-check`; the two `run_migrated_integration` entries run the db_tests landed with this batch — H3).
5. Commit, **atomically with the migration file**: `crates/aero-ai/src/governance.rs` (untracked, already wired — `worker/mod.rs:58` imports it; `cargo test -p aero-ai --lib` = 205 passed including its units), the connector crate if it is still untracked (its drills are the acceptance oracles — verify with `git status` at commit time; do not split the file-gate from its consumers, F11), the `network relay-probe` arm in `crates/aero-cli/src/main.rs` (H2), and the `audit_governance` db_tests module in aero-storage (H3).
6. Gate: `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` (no new warnings) · `scripts/{truth-check,file-size-check,web-check}.sh` (AGENTS §4.3).

## 9. Testable acceptance mapping (the 4 supplied checks → concrete oracles)

| Check | Oracle (concrete, in-repo) |
|---|---|
| **A1 — drill exit codes + verdict lines** | `aero-audit-relay-drill` exits 0 and prints `PASS: N/N delivered (status 2), event_id set-parity exact, stub POSTs N` (relay-drill.rs:57-67 probe → SKIP exit 2 only while table absent; the "round 1 delivered" phrasing belongs to the t11/priority drills). `aero-audit-t11-drill` exits 0 (fail-closed: rows stay status 0, `last_error` records transport failure, zero terminal states). `aero-audit-priority-drill` exits 0 (round-1 `delivered_first == BATCH_SIZE`, moderation `delivered_at` strictly before earliest backlog `delivered_at` — priority-drill.rs:208-231). Harness verdict lines: `b5_check "a3-relay-drill" "PASS"`, `"t11-fail-closed" "PASS"`, `"moderation-priority-drill" "PASS"`, `"audit-provision-check" "PASS"` (legs D/C grep: `outbox-0239: table=audit_governance_outbox pending=[0-9]+ claimed=0 delivered=0 dead=0`, `dead=1`, `delivered=0`, `verdict: fail-closed`). SKIP-with-0239 (e.g. 0239 without B5-3) must be FAIL, never SKIP |
| **A2 — `moderation_finalize_outbox_parity` with vacuous-green guard** | The test landed with this batch (`crates/aero-storage/src/audit_governance.rs`, `db_tests::moderation_finalize_outbox_parity` — the in-tx oracle, F9). This slice's obligations: (a) the file-gate flips it from SKIP to run; (b) the harness empty-filter guard (`run_migrated_integration`, test-integration.sh:197-201 — fails on `running 0 tests`, requires `test result: ok. [1-9][0-9]* passed`) makes a not-yet-existing test a red, not a green; (c) the DDL contract in §4 is exactly what the parity halves (1)-(9) pin (event_id 1:1 join — **creation-time-only**, see §4 note: the join is asserted at commit, not against post-retention state; status 0, class='admin', priority=100, payload `action` = `admin.content.flag`, `UNIQUE(event_id)` dedup) |
| **A3 — governance.rs unit tests** | `cargo test -p aero-ai --lib` (no PG): `moderation_lane_preempts_backlog_under_desc_claim`, `unknown_local_token_passes_through_unmapped` (+ `"message.deleted"`), `user_delete_token_stays_out_of_admin_lane`, `mapping_is_token_keyed`, `outbound_action_is_single_contract_token`, `admin_class_rows_never_aggregated` — 205 passed today including these; must stay green after the commit |
| **A4 — connector suite** | `cargo test -p aero-audit-connector --lib --tests`: currently 25 passed / 1 ignored (fake-clock state machine, claim validation, backoff tests). "24/24" is an out-of-repo contract label — the in-repo oracle is the full `cargo test` result; do not hardcode a count in the harness |

## 10. Risks / sequencing

- **Untracked baseline**: `governance.rs` + the whole `aero-audit-connector` crate are untracked (previous batch artifacts). Commit them with the migration in the same commit — the acceptance oracles live in them (F11).
- **Sibling overlap (reconciled at landing, H3)**: the storage direction's `audit_governance.rs` **repo** is still sibling-owned, but its db_tests landed here so the two `run_migrated_integration` entries go green the moment 0239 does. The sibling adds the repo to the same `audit_governance` module and keeps the parity test's name/literals stable (it is now a pinned contract fixture; moving or renaming it breaks the harness filter). Both directions must `git reset --hard master` (or the campaign baseline) before integrating (AGENTS §4.1).
- **Priority-drill red window**: from this migration landing until B5-3 lands, `moderation-priority-drill` is **red by design** (the honest G6 signal). Do not "fix" it by seeding priorities — that is B5-3's reconciliation (100/200 vs 10/100).
- **Do not extend scope**: no v1 redirect, no backlog-class enqueue — those are the siblings' slices (H4 carries the redirect's dedup obligation). This migration lands the table, the additive trigger, the fixture-authority commit, and the two seam reconciliations (H2/H3); everything else stays SKIP-gated until its own slice lands.
- **No disabled-window backfill (decision, §5)** — **SUPERSEDED (2026-08-08)**: the security review triggered the §5 re-open condition; migration 0241 `aero_reconcile_governance_audit` + relay-tick wiring land with the batch (landing design doc §7.1). v2 now has the v1-equivalent reconciler: enabled-binding join, NOT EXISTS backfill, token-keyed (`message.moderated` only — never fabricates `admin.content.flag`), idempotent (`ON CONFLICT (event_id) DO NOTHING`), runtime-gate-free, dead rows never resurrected.

## 11. Cross-slice handoff notes (landing-time reconciliations, review-flagged)

Four handoff obligations were flagged by the pre-landing reviews (audit-integrity / database / integration). All four are reconciled in this landing; read them **before** landing the sibling slices.

| # | To | Handoff |
|---|---|---|
| **H1** | B5-3 (aero-bus) | **`priority` DEFAULT is 10, not 0.** `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` pins `priority SMALLINT NOT NULL DEFAULT 0` (backlog lane; lines 145/193). The fixture authority — `crates/aero-ai/src/governance.rs:33` `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` (doc: "also the 0239 column default"), committed by **this** slice — settles it at **10**, and migration 0239 (`DEFAULT 10`) is now the SQL truth (pinned behaviorally by `audit_governance::ddl_contract_defaults_and_checks`). Reconcile the aero-bus doc at B5-3 landing: DEFAULT 0 → 10. The priority drill's lane literals 100/200 are the drill's own reconciliation values (B5-3's claim ordering, not this slice's defaults) — leave them. |
| **H2** | B5-2 (probe) | **`network relay-probe` is already wired.** This slice added the `relay-probe` arm to `aero-cli`'s `Network_` command (`crates/aero-cli/src/main.rs`) exactly per `docs/design/2026-08-07-aero-cli-b5-2-relay-probe.design.md` §2.2: `AERO_RELAY_PROBE_BIN` env override else `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`, optional `[mock-url]` passthrough, stdout/stderr inherited (the harness's named `probe: <name>: PASS` greps see the probe's output verbatim), 120s timeout + kill, exit 0/1/2 propagated (contract-outside code → 1). B5-2 lands **only the probe bin file** (`crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs` — the harness `[ -f ]` gate); the CLI arm, the harness entry, and the 9-line grep already work — without this arm the gate would fail red with `use: network ping|dns|port`. |
| **H3** | storage direction | **The two `run_migrated_integration` entries' db_tests landed in this batch.** `crates/aero-storage/src/audit_governance.rs` (registered in lib.rs) carries `audit_governance::db_tests`: `ddl_contract_defaults_and_checks`, `moderation_finalize_outbox_parity` (the harness-named A1 parity test), `moderation_finalize_runtime_disabled_commits_1_plus_0`, `non_moderation_action_passes_through_unmapped`, `duplicate_event_id_is_deduped_by_on_conflict`. The sibling's `AuditGovernanceOutboxRepo` + B5-3 claim ordering still land in their own slices — add the repo to the same `audit_governance` module and keep the parity test's name/literals stable. |
| **H4** | storage direction (v1→v2 redirect) | **The redirect needs its own `ON CONFLICT (event_id) DO NOTHING`.** Trigger firing order is alphabetical ('g' < 's'): `audit_events_governance_enqueue` (0239) fires **first**, `audit_events_snaplink_delivery` (0236) second. When the sibling's `CREATE OR REPLACE` redirect makes the 0236 path also write v2, the redirect's INSERT runs **after** 0239's — a plain INSERT would be the statement that raises duplicate-key on `event_id` and aborts unrelated audit transactions (F6). The 0239 trigger's own `ON CONFLICT` only protects it from earlier duplicates; the sibling's later INSERT must carry its own `ON CONFLICT (event_id) DO NOTHING` (pinned by `duplicate_event_id_is_deduped_by_on_conflict`). |

### 11.1 Reviewer-flagged notes already adopted upstream (pointer only)

The pre-landing reviewers' remaining findings were already folded into this doc by the design step — no further action: disabled-window no-backfill decision (§5 bullet + §10 bullet), retention-sweep creation-time-only invariant (§4 contract note + §6.10 + F12), R2 trigger/function naming (corrections C5), `priority` SMALLINT deviation (C6), payload-envelope supersession (C7), relay-drill PASS-line wording (§9 A1 row).
