# Database Architect Review — R-D2: `message.deleted` into the Governance Outbox (Rust writer)

**Scope**: store inventory (hot vs durable + stock-binary wiring) and a DB-level review of
`docs/design/2026-08-15-aero-storage-r-d2-message-deleted-outbox.design.md` (Proposed).
**Method**: source + migration + boot-path inspection of revision `HEAD` (`e48a3c9`); every
load-bearing design claim re-checked against the live tree; SQL DDL read for CHECK/PK/index
semantics; time-0.3.41 formatter source read for wire-format pinning. Material claims labeled
**Verified / Partial / Missing / Proposed / Unknown** per `prompts/README.md`.

**Verdict up front**: the design is **sound and landable as specced** — all 17 load-bearing
evidence claims verified (three non-material line drifts, confirmed); the fail-closed in-tx
writer mirrors trigger-abort semantics; no schema change; the relay/connector atomic
consume/settle machinery is fenced and single-clock-domain. Four findings: one **Medium**
retention gap the design explicitly defers (unbounded producer into a never-swept table), one
**Medium** acceptance-criteria landmine (AC-3 `occurred_at` spelling assertion will fail as
written — Rust rows spell `Z`, trigger rows spell `+00:00`), two **Low** (partitioned
re-select cost; F8 envelope-wrap claim inaccurate), plus pre-existing Info notes.

---

## 1. Store inventory — durability, implementation, stock wiring, consistency

| Store | Purpose | Durability | Implementation | Stock-binary wiring | Consistency requirement |
|---|---|---|---|---|---|
| PostgreSQL | Source of truth: messages, rooms, audit trail, all outboxes/queues | **Durable** (WAL) | `sqlx::PgPool`, `statement_timeout='10s'`, no isolation override → **READ COMMITTED** (Verified: `aero-storage/src/db.rs:31`); 246 embedded migrations | **Wired unconditionally** (`bin/boot/persistence.rs`, `sqlx::migrate!`) | Strong; outboxes at-least-once |
| `audit_events` | Append-only workspace audit trail (R-D2 source row) | Durable | RANGE-partitioned by day (0146), PK `(id, created_at)`, `(workspace_id, id DESC)` listing index; daily partition DROP via `ensure_audit_event_partitions(keep_days=365 default)` (Verified: 0146, `audit.rs:104`) | Wired via `AuditRepo` in every audited write path | Append-only; DELETE only legal-hold-guarded sweep + partition DROP (0246 header) |
| `audit_governance_outbox` | v2 governance outbox: 1:1 with `audit_events.id`, status machine 0/1/2/3 | Durable | 0239: PK `event_id`, CHECKs (`class IN ('admin','message','room')`, `priority > 0`, `status IN (0,1,2,3)`, payload object); two partial due indexes (0239 FIFO + 0240 priority-DESC); claim `FOR UPDATE SKIP LOCKED` + fenced settle (Verified: `connector/pg.rs:98-190, 195-225, 265-280`) | **Relay wired conditionally** in stock binary: `bin/main.rs:245-264`, presence-gated on `AERO_AUDIT_TOKEN_ENDPOINT`; any stray `AERO_AUDIT_*` with incomplete config → boot error (fail-loud). Harness boots without it (no relay leg, by design) | At-least-once; single clock domain (`clock_timestamp()`); dead at `attempts >= PERMANENT_DEAD_AT=2` for permanent class (≤1 retry) |
| `snaplink_delivery_outbox` (v1) | Legacy 0235/0236 trigger outbox | Durable | Untouched by R-D2 | Wired via 0236 trigger | Untouched |
| `event_outbox` / side-effect outboxes | Room-event realtime fan-out, bot/push/webhook delivery | Durable | Leased claim loops, idempotent keys | Wired unconditionally (`boot/background.rs`) | At-least-once; **has retention sweep** (`sweep_event_outbox`, retention.rs:461) |
| Redis | Hot cluster state (presence, viewer counts, rosters, seq) | **Hot** (appendonly = recovery aid) | fred 9, TTL zsets | Wired unconditionally | Fail-open at every call site |
| NATS JetStream | Cross-instance event log | Durable (room), ephemeral (live) | async-nats 0.36 | Wired unconditionally | At-least-once + seq |
| Audit relay process | Outbox → external governance sink | — | `aero-audit-connector` (`AuditRelay::spawn`, poll 5s default, drain on cancel) | See above: **stock binary wires it when env-complete** | Delivery = fenced 202 + receipt; permanent class dead ≤1 retry |

**Task question answered**: the two stores R-D2 touches are **durable** and both are **stock-wired**
— `audit_events` via `AuditRepo` on every delete (already production), the outbox consume side via
`bin/main.rs` relay spawn (Verified). The R-D2 writer adds the outbox *produce* side to the same
durable transaction; no new backend.

---

## 2. Findings (severity-sorted)

### M1 — `audit_governance_outbox` has no production retention sweep; R-D2 adds the highest-volume producer (Verified, deferred by design)
**Evidence**: `boot/retention.rs` sweeps 20+ tables (messages, notifications, audit rows +
partitions, ai_jobs, webhook_logs, login events, event_outbox, receipts, …) — **none for
`audit_governance_outbox`** (nor v1 `snaplink_delivery_outbox`); the only DELETEs are test/drill
resets (`connector/pg.rs:371,477,570,675`). 0246 header: "outbox rows are never deleted while the
relay runs (v1 precedent)". Today's producers are bounded (moderation budget, 1 L1 window row/ws/min,
room/recall events); the design (Verified §2.3, gate-free per 0242 D2 precedent) makes **every user
message delete** a permanent 1:1 row.
**Impact**: (a) unbounded heap growth proportional to delete volume, in *every* deployment (rows are
written even when no relay is configured); (b) retention asymmetry: the source trail
(`audit_events`) is dropped by partition DROP after `keep_days` (default 365) while the outbox row —
whose payload contains the **120-char content digest** (`authorization.rs:510-516`) — survives
indefinitely; (c) GDPR-erasure gap: message `searchable_text` is wiped on soft delete, but the
digest copy in the outbox payload is not covered by any sweep (design §9 explicitly defers).
**Recommendation (required follow-up, not a blocker for this commit)**: settled-row sweep modeled on
`sweep_event_outbox` (retention.rs:461) or `sweep_consumer_event_receipts`: `DELETE ... WHERE status IN (2,3) AND (delivered_at < now() - N)` with a terminal-state guard so in-flight leases
(status 1) and the 0246 "never delete while relay runs" invariant are untouched; or partition by
`created_at` like `audit_events`. GDPR erasure must additionally scrub/expire outbox payload digests
for erased messages (design already lists this as out of scope — confirm a tracked follow-up).
**Validation**: after deploy, `SELECT status, count(*) FROM audit_governance_outbox GROUP BY status;`
trended; assert the sweep removes only status 2/3 older than N.

### M2 — AC-3 "occurred_at = PG `to_jsonb(created_at)` spelling" will fail for Rust-written rows (Verified root cause)
**Evidence**: `delete_lane_outbox_parity` (AC-3) mirrors `recall_lane_outbox_parity`
(`db_tests/lanes.rs:245`, `:64` PG-spelling assertion) — but the recall lane is **trigger-written**
(PG jsonb spelling `...+00:00`, microseconds), while the delete lane is **Rust-written** via
`governance_envelope` (`outbox.rs:568-626`), which formats with `time` 0.3.41 `Rfc3339`. Formatter
source (`time-0.3.41/src/formatting/formattable.rs:230-257`): UTC offset emits **`Z`**, and
fractional seconds are **trailing-zero-trimmed** (`.123457` for 123457000ns, `.12` for 120000000ns,
omitted for 0). PG `to_jsonb(created_at)::text` emits `+00:00` with fixed 6-digit microseconds when
non-zero. So Rust rows spell `2026-08-15T08:25:56.123457Z` ≠ trigger rows `...123457+00:00` — the
assertion as copied will fail on first run.
**Impact**: CI failure of the deliverable as specced (release blocker for this commit, not
production); plus a real **wire-format inconsistency between the two producers of the same table**
(auth pairs already ship `Z`; moderation/L1/room/recall ship `+00:00`). Both are valid RFC3339, so
an RFC3339-parsing sink is unaffected; a text-dedup/hash sink would see different spellings for the
same instant. The auth lane already handles this by asserting **semantically**
(`db_tests/auth.rs:58-78`: RFC3339-parse + 2µs window vs re-selected `created_at`).
**Recommendation (required)**: AC-3 must assert `occurred_at` the auth.rs way (semantic), not the
recall lane's PG-text way; keep the field-by-field 16-key assertions for the other 15 keys. Optionally
record the producer split (`Z` vs `+00:00`) in the 0239 header/connector doc as the pinned wire
contract for both spellings.

### L1 — Writer re-select `SELECT created_at FROM audit_events WHERE id = $1` is an O(#partitions) probe on the interactive delete path (Verified)
**Evidence**: `audit_events` is RANGE-partitioned by day with PK `(id, created_at)` (0146); a filter
on `id` alone can never prune (partition-key rule for unique indexes) → the planner Append-scans
every daily partition's PK index (~365 with `keep_days=365` default). The design adds this to the
user-delete tx (already the pattern in `write_pair`, `outbox.rs:261-275` — production auth path).
**Impact**: one extra statement per delete; ~1-10ms warm worst case, grows linearly with partition
count. Acceptable today (production precedent), but the delete path is per-user-action at potentially
far higher volume than login-failure pairs.
**Recommendation (optional)**: keep for parity; measure delete-path p95 after landing. If it shows,
the clean fix is returning `created_at` from `AuditRepo::append_in_tx` (signature pin `audit.rs:127`
is design-pinned — a deliberate trade; document it). A `UNIQUE(id)`-only index is impossible on the
partitioned parent.

### L2 — F8 row is inaccurate: `governance_envelope` has no `json!` wrap and no sanitizer parity (Verified)
**Evidence**: `outbox.rs:612` embeds `"payload": detail` **verbatim**; the SQL trigger path wraps +
sanitizes via `aero_snaplink_audit_payload` (0236: key redaction for password/token/secret/…,
64KB cap with sha256 omission) — the "pass-through mirroring" docstring (`outbox.rs:558-560`) is not
byte-exact. A non-object `detail` is not wrapped; it would be rejected by the 0239 payload-object
CHECK → **fail-closed delete abort** (safe, but not the "always object" the F8 row claims). Today
both production callers pass the controlled `{room_id, digest}` object, so no exposure; a future
system caller passing sensitive key names through `soft_delete_outboxed_system` + delete token would
bypass SQL redaction.
**Recommendation (required, small)**: copy `write_pair`'s contract pre-check (reject non-object
detail, fail-closed) into `append_message_delete_in_tx`, and correct the F8 row; note the
sanitizer-asymmetry in the method doc.

### I1 — Pre-existing: auth-pair rows carry `source_system = "aero-auth"` while the relay's PayloadGuard compares against `AERO_AUDIT_SOURCE_SYSTEM` (Verified, out of scope)
**Evidence**: `tokens.rs:75` `AUTH_SOURCE_SYSTEM = "aero-auth"`; `client.rs:514-521` requires
payload `source_system == config.source_system`; deployment invariant is `aero-im.source`
(L1 receipt-contract-resolution doc; drills at `pg.rs:387+`). Under the invariant, auth rows would be
PayloadGuard-dead. Not introduced by R-D2 (delete lane correctly uses `AUDIT_SOURCE_SYSTEM`,
matching 0245/0246 `'aero-im.source'` literals — Verified at 0245:121, 0246:146). Flag for the auth
slice owner; the delete lane's "same exposure as 0242/0245/0246" claim is accurate.

### I2 — Mixed-version rolling window (Info)
Old binaries (pre-commit) write no delete rows; new binaries write them. The `audit_events` trail is
complete in both versions (delete is always audited) — only outbox replication is partial until
rollout completes. No schema interaction; 0245/0246 headers already declare the R-D2 carve-out
(Verified 0245:68-70, 0246:79-80), so no doc contradiction ("Rust NEVER writes the outbox" stays
true for the trigger-owned token set).

---

## 3. Query / index / transaction analysis

### 3.1 Delete transaction (the demonstrated atomic path, after R-D2)
`ImService::delete_message` (im-core `messages.rs:430`) → `soft_delete_outboxed_authorized`
(`authorization.rs:469`) → `soft_delete_locked_outboxed_in_tx` (`events.rs:299`), one READ
COMMITTED tx, lock order:

1. `resolve_message_target` (read) → `lock_effective_message_write_access` (room-access fence) →
   `lock_message_in_tx` = `SELECT ... FOR UPDATE` on the message row (`events.rs:388-400`) — **the
   serialization point**: concurrent deletes of the same message queue here; the loser sees
   `deleted_at IS NOT NULL` → `Ok(None)` → no audit/outbox row (Verified F4 path).
2. `UPDATE messages SET deleted_at=NOW(), blocks='[]', searchable_text='', embedding=NULL, version+1`
   → blob association cleanup + `enqueue_unreferenced_blobs_in_tx` (delete-then-ack stays inside the
   tx).
3. `AuditRepo::append_in_tx` (`audit.rs:127`) — INSERT `audit_events`; fires AFTER INSERT triggers
   0236 (v1), 0239 (pass-through for `message.deleted` — gate `NEW.action <> 'message.moderated'`
   returns early, 0239:80-83), 0242 (`NOT IN ('message.create','message.edit')` pass-through,
   0242:82), 0245/0246 (pass-through). **No double-enqueue possible** (Verified all four gates).
4. **NEW** R-D2 writer (same tx, after the audit append — F1 ordering preserved): re-select
   `created_at` (partitioned scan, L1) → `governance_envelope` → `INSERT ... ON CONFLICT (event_id)
   DO NOTHING` with class `'message'`/priority 10 — both admitted by 0239 CHECKs (Verified 0239:32-35).
   Failure aborts the whole delete (fail-closed, F2/F3) — identical net effect to the 0239 trigger
   RAISE path.
5. `insert_room_event_in_tx` (RoomEvent::Deleted → `event_outbox`) → commit.

No new deadlock class: the relay's `FOR UPDATE SKIP LOCKED` claim locks only committed rows
(uncommitted writer rows invisible), and the 0239 trigger already inserts outbox rows in-tx in the
same statement order. `available_at = clock_timestamp()` (statement time) means the row is claimable
immediately after commit — no gap. At-most-one holds via `deleted_at` guard + `ON CONFLICT`
(composite PK `(id, created_at)` admits same-id re-inserts — the conflict target is `event_id`
itself, so dedup is exact).

### 3.2 Claim/settle (the consume side, unchanged by R-D2 — Verified)
`claim_due` (`pg.rs:98-174`): two-arm CTE — arm A top `limit−K` of
`ORDER BY priority DESC, available_at, created_at, event_id` (matched exactly by the 0240 partial
index → LIMIT pushdown, no per-tick sort); arm B ≥K lowest-lane rows (`priority = MIN(priority)`,
today 10 — delete rows land here alongside L1/recall rows; under admin/100 flood they still get ≥K
rows/round). `FOR UPDATE SKIP LOCKED`, single clock domain (`clock_timestamp()` everywhere).
Settle/requeue/dead are **fenced** (`WHERE claim_token = $2 AND attempts = $3 AND status IN (0,1)
AND lease_expires_at > clock_timestamp()`, `pg.rs:195-225, 265-280`) — a stale claimer can never
overwrite a newer claim; lease expiry re-exposes only the row. Permanent-class (PayloadGuard /
ReceiptMismatch, `client.rs:175, 212`) dead at `attempts >= 2` (≤1 retry, `relay.rs:47-56`); transient
errors re-park with `audit_backoff` (2^n, cap 300s). R-D2 rows are byte-identical in shape to
existing class-`'message'` rows → **zero connector/relay changes** (Verified `claim_due` is
class-agnostic).

### 3.3 Hot-path cost summary (R-D2 delta per user delete)
+1 partitioned re-select (L1) + 1 outbox INSERT (heap append + 2 partial-index entries; index churn
on claim/settle). No new locks beyond the already-held message/room locks; no contention with the
relay (MVCC). The 0242 L1 window is untouched (`message.deleted` never folds — allowlist exact,
Verified 0242:82; AC-2a pins SUM parity unchanged).

---

## 4. Safe migration sequence

**No SQL migration** (Verified: `ls migrations/*.sql | wc -l` = 246; 0239 CHECKs already admit the
row). Pure code deploy:

1. **One commit**: leaf const + pin test (`audit.rs`), `append_message_delete_in_tx`
   (`outbox.rs`, + contract pre-check per L2), choke-point wiring (`events.rs:336-347`), standalone
   seam (`crud.rs:377-384`), literal sweep (production sites: `authorization.rs:514`, `crud.rs:378`;
   test sites: `events.rs:622`, `audit.rs:547`, `governance.rs`, idempotency/parity/lanes/l1), AC-1/2/3
   test surface, `truth-check-lib.sh:164` `L1_TOKEN_LITERALS_CS += '"message.deleted"'` (tests
   skipped by rule 3e — Verified the only non-test literal sites are the two swept).
2. **Validation gates** (in order): `cargo check --workspace` → `cargo test --workspace --lib` →
   `cargo clippy --workspace --all-targets` (no new warnings) → `scripts/{truth-check,file-size-check,
   web-check}.sh` → `scripts/test-integration.sh` (slots `audit_governance::` ≥38+new,
   `moderation_finalize_outbox_parity`, `l1-aggregation-drill`, `moderation-priority-drill`, b5-pin)
   → `scripts/test-notification-fanout.sh` (im-core `db_tests::` incl. new `drill_*` relay leg) —
   throwaway migrated DBs, `--test-threads=1`.
3. **Compatibility window**: no schema/DDL dependency → old and new binaries interoperate with the
   table in either direction; during rollout the sink sees delete rows only from new nodes (I2).
   No backfill (rows are produced forward-only; backfilling historical deletes would fabricate
   audit rows — explicitly wrong, and the design correctly does not).
4. **Rollback**: revert the commit (table schema unchanged, no migration arbiter). New binaries stop
   writing delete rows; existing rows remain claimable by any relay version (0245/0246 rollback
   precedent: never delete outbox rows while the relay runs).
5. **Post-deploy data-integrity checks (operator SQL)**:
   - 1:1 parity: `SELECT count(*) FROM audit_events a WHERE a.action = 'message.deleted' AND
     NOT EXISTS (SELECT 1 FROM audit_governance_outbox g WHERE g.event_id = a.id);` → 0 for
     post-deploy rows (older deletes legitimately have no row).
   - Lane shape: `SELECT class, priority, status, count(*) FROM audit_governance_outbox GROUP BY 1,2,3;`
     → delete rows must be class `'message'`, priority 10, status 0→1→2.
   - Relay health: `SELECT count(*) FROM audit_governance_outbox WHERE status = 3 AND
     last_error LIKE '%PayloadGuard%';` → 0 (wire-contract drift alarm; `source_system` mismatch is
     the first thing to break).
   - L1 regression: `scripts/test-integration.sh` slot `l1-aggregation-drill` (SUM parity) stays
     PASS; `moderation-priority-drill` stays PASS (admin/100 preempts message/10).

---

## 5. Unknown volume, retention, recovery assumptions

- **Delete volume**: no measurement in the tree (no metric on `soft_delete_*` throughput). The M1
  growth rate and the relay's ability to drain (5s poll, batch 100, concurrency 4 defaults) depend
  on it. Required measurement: `MESSAGES_DELETED_TOTAL`-style counter + outbox status gauge trend
  (the B5-4 observability slice is the natural home).
- **Relay presence**: whether deployments set `AERO_AUDIT_TOKEN_ENDPOINT` (harness does not). Without
  it, delete rows accumulate indefinitely (M1).
- **Sink obligations**: governance-sink retention/dedup horizon, whether the sink re-delivers from
  its own store (the local row is the only re-delivery source), and receipt retention — unknown.
- **Audit retention**: `AERO__SERVER__AUDIT_RETENTION_DAYS` default 365 (`ensure_audit_event_partitions
  keep_days`) — the source trail drops while the outbox copy persists (M1 asymmetry).
- **Clock skew**: `audit_events.created_at` is stamped in **Rust** (`audit.rs:152
  OffsetDateTime::now_utc()`), not the DB clock; multi-node skew shifts `occurred_at` (claim order
  uses the DB clock `available_at`, so relay fairness is unaffected). Rows with `created_at` >3 days
  ahead (maintenance `ahead=3`) land in `audit_events_default`, which partition DROP never reclaims —
  pre-existing, worth documenting.
- **Digest privacy**: 120-char content snippets leave the platform in the sink payload (by design,
  `confidential` classification); the sink's retention is outside this repo's control — stated, not
  assumed safe.

---

## 6. Verification ledger (design §0 claims vs live tree)

| Design claim | Result |
|---|---|
| `authorization.rs:514` `Some("message.deleted")`, detail `{room_id, digest}` (digest = first 120 chars) | **Verified** (`authorization.rs:469-528`; digest at :510-516) |
| `crud.rs:378` `soft_delete_audited` appends `"message.deleted"`; no production callers | **Verified** (only tests/db_tests) |
| `events.rs:622` literal inside `audit_failure_rolls_back_delete_and_outbox_append` | **Verified** (fn at :614, literal :622) |
| Choke point `soft_delete_locked_outboxed_in_tx` (events.rs:299-362), append at :336-347, `AuditId` discarded | **Verified** (append block :336-347; `AuditId` return unused) |
| 0245/0246 headers declare `message.deleted` NOT trigger-owned | **Verified** (0245:68-70, 0246:79-80) |
| `AuditGovernanceOutboxRepo` landed: `append_in_tx`/`write_pair`/`governance_envelope`/`is_fail_open_error` | **Verified** (outbox.rs:96, 261-291, 568-626, 636; `write_pair` re-selects `created_at`) |
| 0242 allowlist exactly create/edit; no runtime gate | **Verified** (0242:82; no `snaplink_commercial_runtime` read in 0242) |
| No `LOCAL_ACTION_MESSAGE_DELETED`; pin test bare literal at audit.rs:547 | **Verified** |
| `AuditRepo::append_in_tx -> Result<AuditId, sqlx::Error>` | **Verified** (audit.rs:127) |
| `governance_lane_for("message.deleted") == None` pinned | **Verified** (governance.rs:189) |
| Relay `PERMANENT_DEAD_AT=2`, `PayloadGuard`/`ReceiptMismatch`, `validate_delivery_payload` :514, `receipt_event_id_matches` :539 | **Verified** (relay.rs:47; client.rs:64/175/212/514/539) |
| `audit_governance::` slot = 38 test fns | **Verified** (38 `#[tokio::test]` under the module: 37 ignored + 1 unit) |
| `truth-check-lib.sh:164` `L1_TOKEN_LITERALS_CS` lacks `"message.deleted"` | **Verified** |
| im-core :645 / ai worker :397 pass `message.moderated` | **Verified** (both structurally excluded by exact-token gate) |
| 0239 CHECK admits `'message'`/10 | **Verified** (0239:32-35) |
| Migration count 246 | **Verified** |
| `recall_lane_outbox_parity` at lanes.rs:245 | **Verified** (:245) |
| Drifts (audit append :344-353→:336-347; `soft_delete_outboxed_system` :267; `PERMANENT_DEAD_AT` semantics) | **Confirmed non-material** |
| `append_message_delete_in_tx` design shape (re-select → envelope → append) | **Verified** byte-mirrors `write_pair` (outbox.rs:261-291) |
| Wire contract: `AUDIT_SOURCE_SYSTEM = "aero-im.source"`; delete-lane matches 0245/0246 SQL literals | **Verified** (audit.rs:182; 0245:121, 0246:146) |
