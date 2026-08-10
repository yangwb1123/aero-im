# Requirements Spec — B5-1 audit transport seam in aero-bus: `AUDIT_EVENTS` stream on `audit.ws.*` (per-workspace subject ⇒ per-subject monotonic seq) + exactly-N expansion contract

- **Module**: `crates/aero-bus`
- **Direction**: "Land the B5-1 audit transport seam in aero-bus: AUDIT_EVENTS stream on audit.ws.* (per-workspace subject ⇒ per-subject monotonic seq) + exactly-N expansion contract" — contract item (1) requires outbox + in-tx audit writes with high-volume events L1-aggregated and transported per-workspace; the PG half landed (`0239_audit_governance_outbox.sql` with status 0/1/2/3 + class/priority/delivery_mode), but the bus half is unlanded: no audit subject namespace exists (grep `audit.ws`/`audit.events` → 0 hits), so an L1-aggregated outbox row (N constituents) has no per-workspace subject with monotonic seq to publish exactly-N frames onto.
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-f74336c1.json` (direction index 1; value 8 / risk-reduction 7 / effort 4 / confidence 8)
- **Sibling specs**: `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` (broader B5-1: BR1–BR6, DR1–DR5, A1–A4 — this spec is its aero-bus transport slice, superseding its §4/§6/§8 for this module); `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (disjoint `audit.priority.*` lane); `docs/requirements/2026-08-07-aero-bus-live-nats-acceptance-suite.req.md` (`bus-audit-events` harness leg)
- **Design**: `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` (pins the API shape this spec requirements-izes — §2 constants/function names, §4 expansion contract, §8 test names)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-08 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-bus/src/seq.rs` — `stamp_seq`/`stamped_event_bytes`/`extract_seq` + tests | ✅ **Verified**. `stamp_seq` :31 (top-level `"seq"` sibling key, no-op on `None`/non-object), `stamped_event_bytes` :41 (serialize→stamp→bytes), `extract_seq` :54, traceparent twins :63/:74. Tests: `stamp_inserts_top_level_seq_next_to_kind_tag`, `stamped_payload_still_round_trips_as_room_event` (serde ignores unknown key), `none_seq_leaves_payload_unstamped`, `non_object_payloads_pass_through_untouched`, `extract_rejects_non_u64_stamps`. Generic over any `Serialize` — reusable unchanged for audit frames. |
| E2 | `crates/aero-bus/src/jetstream.rs` — `bootstrap`, `subscribe` prefix chain, `validate_publish_subject`, `poison_safe_pull_config` | ✅ **Verified**. `bootstrap()` :237 declares **4** streams — `IM_MESSAGES` (`im.room.*`, Limits, 7d, File), `IM_EVENTS` (`im.events.>`, Limits, 30d), `AI_QUEUE` (`ai.queue.*`, WorkQueue, 1d), `LIVE_EVENTS` (`live.stream.*`, Limits, 6h, 200k). `subscribe()` :338 maps 4 prefixes via an inline `starts_with` chain (:344–353); unknown prefix → `Err(BusError::Nats(format!("unknown subject prefix: {subject}")))` :353 (fail-closed). `validate_publish_subject` :24 (concrete subjects accepted — test `accepts_well_formed_concrete_subjects`; `*`/`>`/empty/whitespace rejected). `poison_safe_pull_config` :145 (`POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` for durable non-`aero-server-*`, `New` for ephemeral/process-local). **No audit subject namespace exists anywhere** (E10). |
| E3 | `im_messages_stream_config` pattern with `duplicate_window == max_age` | ✅ **Verified**. `im_messages_stream_config` :129 + `IM_MESSAGE_DUPLICATE_WINDOW` (7d); test `im_stream_duplicate_window_covers_outbox_retries` :461 asserts `duplicate_window == max_age` — the broker dedup horizon covers the producer's full outbox retry horizon. The pattern the `AUDIT_EVENTS` config builder must clone. |
| E4 | `idempotent_publish_headers` (`Nats-Msg-Id`) | ✅ **Verified**. :60–80: `validate_publish_subject` + non-empty id + no CR/LF → `NATS_MESSAGE_ID` header. Tests `idempotent_publish_builds_exact_nats_message_id_header` / `idempotent_publish_rejects_unsafe_message_ids`. Broker dedup key = `Nats-Msg-Id` alone (stream-global — safe because constituent event_ids are UUIDs). |
| E5 | `crates/aero-bus/src/traits.rs` — `EventBus` trait | ✅ **Verified**. `publish` / `publish_idempotent` (default falls back to `publish`, JetStream impl overrides with `Nats-Msg-Id` dedup) / `publish_json` (`Self: Sized`, stays dyn-compatible) / `subscribe`. `publish_idempotent_ack` is private (JetStreamBus), returns `PublishAck { duplicate, sequence }` for the live-NATS test. **No new trait method needed** for the audit lane. |
| E6 | `crates/aero-im-core/src/seq.rs` — `SeqProvider` seam (:40 cited) | ✅ **Verified (line drift)**. `pub trait SeqProvider` at **:36** (direction cited :40 — trivial drift, same symbol): `next_seq(subject) -> Option<u64>`, `None` ⇒ publish unstamped (never block). `LocalSeqProvider` (DashMap, process-local, 1-based) + `impl SeqProvider for aero_storage::SeqStore` (Redis `INCR aero:seq:{subject}`, cluster-correct — `crates/aero-storage/src/seq.rs`: `SeqStore` :27, `next` :44). Wired via `ImService::with_seq` / `LiveService::with_seq`. ⚠️ **Dependency direction**: aero-bus is a base crate (depends only on aero-common) — it **cannot import `SeqProvider` from aero-im-core**; seq *composition* (provider + `stamped_event_bytes`) happens in the relay owner. |
| E7 | `crates/aero-storage/src/event_outbox.rs:424` — `assign_seq_if_absent` | ✅ **Verified**. :424: `UPDATE event_outbox SET seq = COALESCE(seq, $2) WHERE id = $1 AND published_at IS NULL RETURNING seq` — concurrent callers converge on one persisted value; the persist-stamp pattern the audit relay clones per constituent. Sibling machinery: `claim_due` :295 (`FOR UPDATE SKIP LOCKED` :333), `mark_published` :447 (fenced on `attempts`), `mark_failed` :476 (fenced re-park). |
| E8 | `crates/aero-server/src/ws/ws_impl/bus.rs:289` — NotifyBatch 1→N precedent | ✅ **Verified, with correction**. NotifyBatch per-recipient expansion at :289–302 (one bus event → one targeted `notify` frame per recipient; avoids O(N) NATS publishes). ⚠️ **Consumer-side fan-out (1→N), never a publish-side N→1 merge** — it is the in-repo precedent for "one durable row → exactly N frames" (which A3 reuses for the relay side), but the **L1 merge (N→≤ceil(N/K)) has no in-repo precedent** — [PROPOSED], owned by the storage/im-core slice (R-3). |
| E9 | `migrations/0239_audit_governance_outbox.sql` — landed DDL (the DR1 half) | ✅ **Verified**. Landed (latest = 0241): `audit_governance_outbox` — `event_id UUID PRIMARY KEY` (= `audit_events.id` 1:1), `status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))` (⚠️ **INTEGER not SMALLINT** — drift vs the design doc's SMALLINT; the values 0/1/2/3 are the normative part), `class` CHECK `('admin','message','room')` DEFAULT `'message'`, `priority SMALLINT CHECK (priority > 0)` DEFAULT 10, `delivery_mode` CHECK `('push')`, `payload JSONB CHECK (jsonb_typeof = 'object')`, lease columns (`claim_token`/`lease_expires_at`) + claim-state CHECK, `attempts`, `available_at`, `delivered_at`, `last_error`, `created_at`; due index `(available_at, created_at, event_id) WHERE status IN (0,1)`; 0240 adds the due+prio index, 0241 reconciles. Connector half: `crates/aero-audit-connector/src/pg.rs` claim already orders `priority DESC` :117; `STATUS_ENQUEUED..STATUS_DEAD` :27–30 single-sourced from `aero_common::model::audit::OutboxStatus` (:20, `= 0..3`, explicit discriminants). |
| E10 | No audit subject namespace exists | ✅ **Verified**. Fixed-string `grep -F "audit.ws."` over `crates/` + `migrations/` = **0 hits**; `AUDIT_EVENTS` stream literal = 0 hits (the only `AUDIT_EVENTS`-shaped hits are the unrelated `AERO_AUDIT_EVENTS_URL` env var in `aero-audit-connector/src/config.rs` — the external sink endpoint, not a bus subject). A publish to `audit.ws.*` today is unrouted (`subscribe()` hard-errors, E2). |
| E11 | `crates/aero-bus/src/lib.rs` doc — 3 streams vs `bootstrap()`'s 4 | ✅ **Verified**. lib.rs :8–12 lists `IM_MESSAGES`/`IM_EVENTS`/`AI_QUEUE` only — missing `LIVE_EVENTS` (stale; BR6 fixes it to 5 including `AUDIT_EVENTS`). |
| E12 | `docs/campaigns/implementation-gate.md:63` (campaign row 1) + G6 | ✅ **Verified**. :63 aero-im row 1: "Outbox + in-tx：DDL（status 0/1/2/3 normative）；`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1 | 30 个忽略测试 CI 全绿（37/37）；P2 parity". :78: **G6 (B5)** = "37/37、T-11、moderation 优先级". The `‡ 类走 L1` row's bus half (per-workspace transport for aggregated rows) is what this direction lands. |
| E13 | (supplementary) design doc pins the API shape | ✅ **Verified**. `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` §2: `pub const AUDIT_SUBJECT_PREFIX: &str = "audit.ws."` (re-exported from lib.rs), extracted `fn stream_for_subject(subject) -> Option<&'static str>` (byte-identical 4 existing branches + `audit.ws.`→`AUDIT_EVENTS`), `fn audit_events_stream_config()` (name `AUDIT_EVENTS`, subjects `["audit.ws.*"]`, Limits, 7d, `duplicate_window = max_age`, **explicit window — an unset one is server-defaulted to 2m**, live-verified against NATS v2.10.29), bootstrap get_or_create + update; §4 exactly-N expansion contract (per-constituent persisted seq, `Nats-Msg-Id` = constituent event_id, publish in ordinal order); §8 test names (`stream_for_subject_audit_case`, `audit_events_stream_config_is_limits_with_7d_duplicate_window`, `live_nats_audit_stream_declared_and_deduplicates`, `live_nats_audit_expansion_publishes_exactly_n_frames`). |

## 2. Verified current state (the pipeline this direction modifies)

```
PG half (LANDED — E9):
  audit_events (L0, partitioned 0146, legal-hold 0154)
    └─ audit_governance_outbox  (0239: status 0/1/2/3 CHECK + class/priority/delivery_mode CHECKs
        + lease/attempts/backoff + due index WHERE status IN (0,1); 0240 prio idx; 0241 reconcile)
         └─ connector claim  ORDER BY priority DESC  (aero-audit-connector/src/pg.rs:117)
              └─ B5-2 connector → HTTP sink   (claim→HTTP loop, no bus hop today)

bus layer (crates/aero-bus, THIS module):
  bootstrap()  ──  IM_MESSAGES · IM_EVENTS · AI_QUEUE · LIVE_EVENTS     (E2: no audit stream)
  subscribe()  ──  4-prefix inline starts_with chain, fail-closed       (E2: audit.ws.* unrouted)
  publish      ──  publish_idempotent(subject, payload, event_id)       (E4/E5: Nats-Msg-Id dedup)
  seq.rs       ──  stamp_seq / stamped_event_bytes / extract_seq        (E1: generic, reusable)
  SeqProvider  ──  LocalSeqProvider / Redis SeqStore (im-core + storage, E6) — composed by relay owner
  lib.rs doc   ──  3 streams listed, bootstrap declares 4               (E11: stale)

1→N precedent ──  NotifyBatch consumer-side fan-out (bus.rs:289)         (E8: NOT a merge)
N→1 merge     ──  NO in-repo precedent                                   (R-3: [PROPOSED], storage/im-core)
```

**Gaps the direction closes** (all verified): (1) no audit subject namespace — an L1-aggregated outbox row (N constituents) has no per-workspace subject with monotonic seq to publish exactly-N frames onto (E2/E10); (2) the generic machinery (seq stamping E1, `SeqProvider` seam E6, persist-stamp `assign_seq_if_absent` E7, idempotent publish E4, poison-safe consumer config E2) is in-module and reusable **unchanged**; (3) the 1→N expansion precedent is consumer-side `NotifyBatch` fan-out (E8) — the publish-side exactly-N contract has no precedent and is pinned by BR5; (4) lib.rs doc is stale (E11).

## 3. Scope

**In scope (this direction, module `crates/aero-bus`)**:
- The **audit subject namespace + stream**: normative `audit.ws.{workspace_id}` (per-workspace subject ⇒ per-workspace monotonic seq — the `im.room.{id}` precedent), declared in `bootstrap()` as `AUDIT_EVENTS` and mapped in `subscribe()` via an extracted pure fn. Single public prefix constant `AUDIT_SUBJECT_PREFIX` = the one change point for the contract literal.
- The **per-subject seq contract for audit subjects**: existing `seq.rs` primitives (E1) + `SeqProvider` seam (E6) — stamped once at publish, per-subject monotonic, gaps legal, redelivery carries the same seq, `None` ⇒ unstamped. aero-bus only provides the primitives; composition with a provider is relay-owned (E6 dependency direction).
- The **exactly-N expansion contract** (bus-side half of the direction's A3): one aggregated outbox row → exactly N published frames, one per constituent, each with its own stable event id (`Nats-Msg-Id`) and its own seq stamp in the same per-subject monotonic sequence; 1:1 (admin-class) rows → exactly 1 frame (the N=1 case).
- **Reuse, no new paths**: `publish_idempotent` + `validate_publish_subject` (E4/E5), `poison_safe_pull_config` for the connector's durable consumer (E2) — no new `EventBus` trait method, no new consumer-config path, no new env vars.
- Doc alignment: `crates/aero-bus/src/lib.rs` stream list (E11 — stale, missing `LIVE_EVENTS`; BR6 rewrites to 5).

**Dependency-owned (acceptance checks preserved here, built by sibling directions — do not build in this module)**:
- 0239 DDL + in-tx writes + `audit_governance.rs` repo → **landed** (E9) — nothing to build, but A1/A3-PG assertions reference it.
- L1 aggregation mechanics (N→≤ceil(N/K) merge, window/K constant, admin-class 1:1 bypass) → **B5-1 storage/im-core slice** — [PROPOSED], no in-repo precedent (E8/R-3); this module only consumes the *result* (a row with N constituents).
- Claim/status pump (0→1 SKIP LOCKED lease, 1→2 only after external ack, 422→3 dead, transient re-park) + per-constituent exactly-N publish loop composing `SeqProvider` + `aero_bus::stamped_event_bytes` + `publish_idempotent` → **the relay owner** (design doc §5.4); the aero-bus live-NATS A3 uses an **in-test relay harness** (local counter, single instance) so the seam is testable without the relay.
- Connector durable subscription on `audit.ws.*` (`DeliverPolicy::All`, poison bounds) → **B5-2 connector** (uses the BR1 constant — no second literal).

**Out of scope**: B5-2 connector crate; B5-3 priority lane (`audit.priority.*` WorkQueue stream — sibling spec, must stay disjoint per BR1/R-1); B5-4 provisioning gate; local→outbound action-token mapping; the `audit_outbox_frame` child-table DDL shape (interface pinned in the design doc §4/§5.1 — this module consumes the contract "per-constituent persisted seq, readable on claim, idempotent to assign", it does not own the DDL); changes to the IM `event_outbox` claim/ordering invariants; `NotifyBatch` fan-out; the v1 snaplink usage relay.

## 4. Requirements

### BR1 — Audit subject namespace mapped in `subscribe()` (load-bearing)
- Normative namespace **`audit.ws.{workspace_id}`** (the direction's "per-subject monotonic seq" made concrete — per-workspace subjects ⇒ per-tenant ordering/dedup, matching the `im.room.{id}` precedent). Expose **one** public constant, `pub const AUDIT_SUBJECT_PREFIX: &str = "audit.ws."`, used by the mapping, the relay's publish path, and the B5-2 connector's subscription — no second literal (design doc §2.1).
- Extract the inline `starts_with` prefix chain (E2) into a pure fn `fn stream_for_subject(subject: &str) -> Option<&'static str>`, returning `Some("AUDIT_EVENTS")` for `audit.ws.*` and the existing stream names for the 4 current families — **byte-identical behavior** for the existing 4 (same literals; same error string).
- Unknown prefixes keep the current fail-closed behavior: `stream_for_subject` → `None` → `subscribe()` returns `Err(BusError::Nats("unknown subject prefix: {subject}"))` (exact existing message, E2) — a typo must never silently land on another stream.
- **Disjointness (sibling B5-3 BR1)**: `audit.ws.*` and `audit.priority.*` are two streams over disjoint subject families (second tokens `ws` vs `priority`); the unit test pins `audit.priority.x` does **not** map to `AUDIT_EVENTS` and `audit.ws.x` does **not** map to `AUDIT_PRIORITY` — branch order is irrelevant (multi-agent parallel integration: hand-merge `jetstream.rs` per AGENTS.md §4.1).

### BR2 — `bootstrap()` declares `AUDIT_EVENTS` with Limits retention
- New stream `AUDIT_EVENTS`: `subjects: ["audit.ws.*"]`, `retention: Limits` (IM_MESSAGES precedent — the audit bus is **transport**; the durable evidence is `audit_governance_outbox` + `audit_events` in PG, E9; Limits keeps redelivery/replay available to the connector's durable cursor), `storage: File`.
- `max_age` = 7d and `duplicate_window` = `max_age` (IM_MESSAGES precedent, asserted by `im_stream_duplicate_window_covers_outbox_retries`, E3) — broker-level `Nats-Msg-Id` dedup covers the relay's full retry horizon (connector backoff cap 300s is far inside 7d; live-verified on NATS v2.10.29: equality allowed verbatim, `>max_age` rejected, dedup survives restart — design doc §0.1). **`duplicate_window` must be set explicitly** — an unset window is server-defaulted to 2m (live-verified on `IM_EVENTS`), silently shrinking the dedup horizon.
- Stream config extracted as a pure builder (`fn audit_events_stream_config()`, mirroring `im_messages_stream_config` E3) so retention/duplicate-window are unit-assertable without a live NATS. Declared idempotently in `bootstrap()` via the get_or_create + update pattern (the update half heals clusters whose stream predates the 7d window — IM_MESSAGES upgrade pattern).

### BR3 — Poison-safe pull config applies unchanged to the audit stream
- `poison_safe_pull_config` is already generic over subject/durable (E2): the connector's durable consumer (name `aero-audit-connector`, does **not** match the `aero-server-*` process-local prefix) gets `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` on first creation (backlog replay); ephemeral consumers get `New`. No new config path, no per-stream exceptions.

### BR4 — Publish path accepts concrete audit subjects, no new API
- `validate_publish_subject` accepts concrete `audit.ws.<uuid>` subjects (same class as the already-tested `ai.queue.summarize`); wildcard `audit.ws.*` remains publish-rejected (E2/E4).
- The audit-outbox relay publishes with the existing `publish_idempotent(subject, payload, event_id)` mechanism (event_id as `Nats-Msg-Id`, E4) — `EventBus` trait: **no changes** (stays dyn-compatible, E5).

### BR5 — Per-subject seq contract for audit subjects + exactly-N expansion
- **Seq contract**: each frame is stamped once at publish via `stamped_event_bytes` (E1) with a per-subject value from the `SeqProvider` seam (E6); a redelivery of the same bytes carries the **same** seq; gaps are legal; `None` ⇒ publish unstamped (never block delivery). Per-constituent persistence uses the `assign_seq_if_absent` pattern (E7: `COALESCE(seq, $2)` + `RETURNING seq`) so the relay and the connector agree on one value; the relay stamps **the persisted value, never its pre-persist candidate** (E7/im-core `publish_claimed_outbox` precedent); a persist that returns no row (fence miss) ⇒ **abort the publish** (the only way to emit a frame the claim owner did not authorize).
- **Expansion contract** (bus-side half of acceptance A3): one L1-aggregated `audit_governance_outbox` row → **exactly N published frames**, one per constituent, in ordinal order, each with its own stable event id (`Nats-Msg-Id`) and its own seq stamp in the same per-subject monotonic sequence — the 1→N shape of `NotifyBatch` expansion (E8) applied publish-side. Non-aggregated (admin-class / 1:1) rows → exactly 1 frame. Per-subject monotonicity across frames of one row and across rows of one workspace is guaranteed by the shared per-subject counter (Redis `INCR` in cluster, E6).

### BR6 — Doc alignment
- `crates/aero-bus/src/lib.rs` stream list rewritten to 5: `IM_MESSAGES` (`im.room.*`, Limits, 7d) · `IM_EVENTS` (`im.events.>`, Limits, 30d) · `AI_QUEUE` (`ai.queue.*`, WorkQueue, 1d) · `LIVE_EVENTS` (`live.stream.*`, Limits, 6h) · `AUDIT_EVENTS` (`audit.ws.*`, Limits, 7d) — crate doc matches `bootstrap()` (E11).

## 5. Acceptance checks (preserved from the direction, made testable)

Ownership tags: **[aero-bus]** = built & tested in this module; **[relay]**, **[B5-2]** = dependency-owned, preserved here verbatim so the drill suite is complete. Test names follow the design doc §8 pins (E13).

### A1 — Mapping, config builder, publish validation, poison config: unit **[aero-bus]**
*Preserves: "`stream_for_subject("audit.ws.<uuid>") == Some("AUDIT_EVENTS")`; unknown prefix still `Err(BusError::Nats("unknown subject prefix"))`; `audit_events_stream_config()` asserts subjects==["audit.ws.*"], Limits retention, duplicate_window==max_age (7d); `validate_publish_subject` accepts concrete audit.ws.<uuid> and rejects the wildcard."*
- **Unit (no NATS, `jetstream.rs` tests module — pattern: `pull_config_bounds_redelivery_for_poison_messages`)**, test `stream_for_subject_audit_case`:
  - `stream_for_subject("audit.ws.<uuid>") == Some("AUDIT_EVENTS")`;
  - the 4 existing prefixes map unchanged (`im.room.x`/`im.events.x`/`ai.queue.x`/`live.stream.x`);
  - unknown prefix → `None`; `subscribe()` on it yields `Err(BusError::Nats("unknown subject prefix: {subject}"))` — assert the **exact existing message shape** (E2: `unknown subject prefix: ` + the subject), byte-identical to today's behavior;
  - **disjointness**: `stream_for_subject("audit.priority.x") != Some("AUDIT_EVENTS")` (and, once B5-3 lands, `audit.ws.x` != `AUDIT_PRIORITY`) — two streams, no overlap (B5-3 spec BR1).
- **Unit**, `AUDIT_SUBJECT_PREFIX == "audit.ws."` — single literal; the constant is `pub` and re-exported from `lib.rs`.
- **Unit**, test `audit_events_stream_config_is_limits_with_7d_duplicate_window` (pattern: `im_stream_duplicate_window_covers_outbox_retries`): `subjects == ["audit.ws.*"]`, `retention == Limits`, `max_age == 7d`, `duplicate_window == max_age` (explicitly set), `storage == File`.
- **Unit**: `poison_safe_pull_config("audit.ws.<uuid>", Some("aero-audit-connector"))` → `durable_name == "aero-audit-connector"`, `DeliverPolicy::All` (name does not match `aero-server-*`), `max_deliver == POISON_MAX_DELIVER`, `ack_wait == POISON_ACK_WAIT`.
- **Unit**: `validate_publish_subject("audit.ws.<uuid>")` → Ok; `"audit.ws.*"` → `BusError::InvalidSubject` (wildcard); CRLF-in-message-id rejection still applies (E4).

### A2 — Seq stamping extension: per-frame stamp, same seq on redelivery **[aero-bus]**
*Preserves: "seq.rs extension: per-frame stamp, same seq on simulated redelivery, gaps legal, None ⇒ unstamped."*
- **Unit (`seq.rs` tests module — extend the existing stamp tests, E1)**:
  - an audit-shaped frame (JSON object with a `kind`-style tag) stamped with seq `s` → `extract_seq` returns `s`; the same bytes re-serialized/re-published (simulated redelivery) still carry `s` — the dedup-key property asserted at the bytes level;
  - `stamped_event_bytes(event, None)` → payload unstamped, `extract_seq == None` (existing test `none_seq_leaves_payload_unstamped` covers the primitive; the audit case adds an envelope-shaped payload);
  - gaps legal: doc-level contract already in `seq.rs` — no test may assert contiguity (the counter may mint-and-crash, E6);
  - a stamped frame still deserializes through serde (unknown `"seq"` key ignored — existing `stamped_payload_still_round_trips_as_room_event` shape).

### A3 — Live-NATS: exactly-N expansion with per-subject monotonic seq **[aero-bus] + [relay harness]**
*Preserves: "Live-NATS A3: relay drains one aggregated row → exactly N frames on audit.ws.<ws>, per-subject seq strictly monotonic across frames and across rows of that workspace, second workspace starts its own sequence; replay of the same outbox row republishes same seq with broker dedup collapsing duplicates (Nats-Msg-Id)."*
- **Live-NATS (`#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]`, `jetstream.rs` tests module — pattern: `live_nats_deduplicates_same_message_id`)**, test `live_nats_audit_stream_declared_and_deduplicates`:
  - `connect(bootstrap_streams: true)` → `get_stream("AUDIT_EVENTS")` exists; `info().config` asserts Limits, `subjects == ["audit.ws.*"]`, `duplicate_window == max_age == 7d` (bootstrap applied the config, not only fresh installs);
  - `publish_idempotent_ack("audit.ws.<nonce>", payload, message_id)` twice → first `duplicate: false`, retry `duplicate: true`, identical `sequence` (E4/E5).
- **Live-NATS**, test `live_nats_audit_expansion_publishes_exactly_n_frames` (in-test relay harness — fixture: one aggregated row for workspace ws with N=3 constituents e1..e3; loop: `seq = local counter next_seq("audit.ws.{ws}")` → `publish_idempotent(subject, stamped_event_bytes(payload, seq), event_id)`; ephemeral subscriber on `audit.ws.{ws}` counts frames):
  - **exactly 3 frames** received (no 4th);
  - `extract_seq` over the received frames is **strictly monotonic**;
  - replay of the same row (re-publish the 3 frames with the same event_ids and persisted seqs) → each re-publish acked `duplicate: true` with the original stream sequence; the subscriber sees **no additional frames** (broker dedup collapses, `Nats-Msg-Id` = constituent event_id);
  - a second workspace `audit.ws.{ws2}` starts its own sequence at 1 (per-subject isolation — frames on ws2 do not continue ws1's counter);
  - stream purged after (pattern: `live_nats_deduplicates_same_message_id` purge).
- The harness uses a **local counter** (single-instance; the real relay's Redis `SeqStore` composition is [relay]-owned per E6 — aero-bus cannot import `SeqProvider`). Cross-row monotonicity within one workspace is asserted by draining two rows (N=3 + N=2) into the same subject and checking the union of seqs is strictly increasing.

### A4 — Full suite green + IM `event_outbox` invariants untouched **[all]**
*Preserves: "A4: existing event_outbox claim/ordering guards untouched; cargo check --workspace clean + cargo test --workspace --lib green + scripts/truth-check.sh 0 violations."*
- `cargo check --workspace` clean; `cargo test --workspace --lib` all green (PG-gated tests via `-- --ignored` + `DATABASE_URL` + throwaway migrated DB — unchanged from today, E9); `cargo clippy --workspace --all-targets` no new warnings; `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations.
- The IM `event_outbox` claim/ordering guards (`claim_due` per-aggregate-version + delivery-ordinal `NOT EXISTS`, E7) and `dispatch_event_outbox_batch` behavior are unchanged — this direction adds no code to aero-storage.
- The `bus-audit-events` leg of the live-NATS acceptance suite (sibling spec) goes green once this seam lands (`AUDIT_EVENTS` declared post-bootstrap; `publish_idempotent` dedup on a concrete `audit.ws.*` subject; durable consumer resumes `DeliverPolicy::All`).

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| A1: `stream_for_subject_audit_case` (mapping, disjointness, fail-closed error string) | `crates/aero-bus/src/jetstream.rs` tests module | pure unit, no NATS — pattern: `pull_config_bounds_redelivery_for_poison_messages` |
| A1: `AUDIT_SUBJECT_PREFIX` literal + re-export | `crates/aero-bus/src/jetstream.rs` tests module | pure unit |
| A1: `audit_events_stream_config_is_limits_with_7d_duplicate_window` | `crates/aero-bus/src/jetstream.rs` tests module | pure unit — pattern: `im_stream_duplicate_window_covers_outbox_retries` |
| A1: poison config + publish validation for `audit.ws.*` | `crates/aero-bus/src/jetstream.rs` tests module | pure unit |
| A2: seq stamping on audit-shaped envelopes (same-seq-on-redelivery, None ⇒ unstamped, serde round-trip) | `crates/aero-bus/src/seq.rs` tests module (extend existing stamp tests) | pure unit |
| A3: `live_nats_audit_stream_declared_and_deduplicates` | `crates/aero-bus/src/jetstream.rs` tests module | `#[ignore]` live NATS at `AERO__NATS__URL` — pattern: `live_nats_deduplicates_same_message_id` |
| A3: `live_nats_audit_expansion_publishes_exactly_n_frames` (in-test relay harness, 2 workspaces, replay/dedup) | `crates/aero-bus/src/jetstream.rs` tests module | `#[ignore]` live NATS |
| A4 | CI + `scripts/test-integration.sh` (`bus-audit-events` leg, sibling spec) | full suite; gate G6 "37/37、T-11、moderation 优先级" |
| A3-PG (≤ceil(N/K), admin bypass), A1-PG (status machine) | aero-storage db_tests (`audit_governance.rs`, landed E9) | PG, `#[ignore]` + `DATABASE_URL` + throwaway DB — dependency-owned, preserved by sibling specs |

## 7. Risks / [PROPOSED] items

- **R-1 — Exact audit subject token is out-of-repo contract text**: the v2 contract docs are not in this repo; `audit.ws.{workspace_id}` is pinned here **normatively for testability** (same stance as the sibling B5-3 spec's `audit.priority.*`). If the contract dictates another namespace, the public `AUDIT_SUBJECT_PREFIX` constant (BR1) is the single change point; the load-bearing property is **per-workspace subjects ⇒ per-subject monotonic seq per tenant**, not the literal token. **Must stay disjoint from B5-3's `audit.priority.*`** (sibling spec BR1) — two streams, two namespaces, no subject can match both.
- **R-2 — `max_age`/`duplicate_window` vs B5-2 horizon**: pinned at 7d (= IM_MESSAGES precedent, `duplicate_window = max_age`). If B5-2's lease/backoff horizon or the sink's idempotency window ever exceeds 7d, the constants must be raised together — coordinated constant, flagged not invented independently. `duplicate_window` must be set **explicitly** in `audit_events_stream_config()` — an unset window is server-defaulted to 2m (live-verified on IM_EVENTS; design doc §0.1 L1). NATS is transport; the PG `audit_governance_outbox` remains the durable source of truth (a crashed/dead connector loses no evidence — Limits retention + `DeliverPolicy::All` replay, BR2/BR3).
- **R-3 — L1 aggregation mechanics are [PROPOSED], owned by the storage/im-core slice**: no in-repo N→1 merge precedent exists (E8 — `NotifyBatch` is consumer-side fan-out; 0179 is a single-row snapshot). K (bucket size), window/flush semantics, and the transaction-safe bucket belong to the B5-1 storage/im-core slice; this module's acceptance only pins the **result**: one row → exactly N frames (BR5/A3). The admin-class 1:1 bypass is the N=1 case of the same contract.
- **R-4 — Exactly-N needs per-constituent persisted seqs (interface, not built here)**: the expansion contract is only sound if a relay crash between seq-assign and publish of frame k reuses the persisted seq (W2/W3 crash-window closure, design doc §0.1 L3). The 0239 child-table interface (`audit_outbox_frame` with `COALESCE(seq, $2)` + `RETURNING seq`, fence-miss ⇒ abort publish) is pinned by the design doc §4/§5.1 — aero-bus consumes the contract; the DDL is dependency-owned. ⚠️ The **landed** 0239 DDL (E9) does not yet contain `audit_outbox_frame` — the frame-table migration is a sibling slice's follow-up; until it lands, A3's live test uses the in-test harness (row fixture + local counter), which does not depend on the DDL.
- **R-5 — Seq provider composition is relay-owned**: aero-bus is a base crate and cannot import `SeqProvider` from aero-im-core (E6). Multi-instance correctness requires the relay to compose the **server's shared Redis `seq_store`** (a fresh `LocalSeqProvider` per instance would mint interleaved duplicate seqs 1,1,2,2… for one workspace subject). The aero-bus live test's local counter is single-instance-only — never cite it as proof of multi-instance monotonicity.
- **R-6 — Error-string precision**: the fail-closed error is `BusError::Nats("unknown subject prefix: {subject}")` (E2) — the acceptance's shorthand `Err(BusError::Nats("unknown subject prefix"))` must be tested as the **full** message shape so the extracted `stream_for_subject` is byte-identical to today's inline chain.
- **R-7 — Evidence drift noted**: `SeqProvider` trait is at `aero-im-core/src/seq.rs:36` (direction cited :40 — same symbol); the landed 0239 `status` column is `INTEGER` (design doc's `SMALLINT` drifted — the CHECK `IN (0,1,2,3)` values are the normative part, single-sourced at `aero_common::model::audit::OutboxStatus`); `event_outbox.rs`/`bus.rs`/`jetstream.rs` line anchors are as-of-verification (files/symbols are the stable anchors).

## 8. Sequencing

1. **This direction (aero-bus)**: BR1–BR6 — pure additive seam (new stream + mapping + constant + seq/expansion contract + docs); no migration, no trait change, no boot wiring; lands independently and is unit/live-NATS-testable today. Unlocks A1, A2, A3.
2. **Relay owner**: claim/status pump + per-constituent exactly-N publish composing `aero_bus::AUDIT_SUBJECT_PREFIX` + `publish_idempotent` + `stamped_event_bytes` + the server's Redis `seq_store` (design doc §5.4) — the 0239 frame-table DDL (R-4) is its prerequisite; unlocks the real-relay half of A3 and A2 of the broader B5-1 spec.
3. **B5-2 / B5-4**: connector subscribes `subscribe(&format!("{AUDIT_SUBJECT_PREFIX}*"), Some("aero-audit-connector"))` — the constant is the single literal (BR1); provisioning gate; the `bus-audit-events` harness leg (sibling live-NATS spec) goes green.
4. **G6 gate**: green when A1–A4 + 37/37 + T-11 + moderation-priority drill all pass (`implementation-gate.md:78`); the `‡ 类走 L1` campaign row (`:63`) is fully landed once the transport seam carries aggregated rows to the connector.
