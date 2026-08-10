# Distributed-Systems Engineer Review — Message Recall (消息撤回)

**Deviation note:** `prompts/README.md` does not exist (no `prompts/` dir in the repo; same finding as the other reviewers). The prompt body was self-contained; the review below was derived from direct source inspection of the feature surfaces (recall tx, outbox relay, bus listener, seq, hub, web replay path) plus the evidence supplied.

**Method.** Every claim below was verified against source on this tree: `authorization.rs` (`recall_outboxed_authorized`/`recall_locked_outboxed_in_tx`/`recall_role_allowed_in_tx`), `events.rs` (`edit_locked_outboxed_in_tx` UPDATE WHERE clause), `query.rs:167-187` (`changes_since` predicate), `service/outbox.rs` (`publish_claimed_outbox`, `materialize_outbox_payload` Recalled arm, `OUTBOX_LEASE=30s`), `event_outbox.rs` (claim/`assign_seq_if_absent`/`mark_published`/`mark_failed` fencing), `seq.rs` (Redis `INCR`), `bus.rs:152-470` (two-phase decode, poison ack-drop, AI-cache purge), `hub.rs` (bounded mpsc, disconnect-on-full), `background.rs:62-97` (relay loop, 250 ms), `unfurl_bot.rs:175-189` (get_version → `edit_outboxed_system`), `web/app.js:324-343` (`applyChange`/`replayChanges`), `web/app.js:526` (cursor seed), `migrations/0238`, `0125`, `0148/0149/0158/0174` (backfill function lineage), `webhooks.rs:383-397` (`recalled` kind mapping).

---

## 1. State map — owner, store, durability, consistency, replication, failover

| State | Owner (writer) | Store | Durability | Consistency | Replication | Failover / recovery |
|---|---|---|---|---|---|---|
| `messages` row: placeholder `blocks`, `searchable_text=''`, `embedding=NULL`, `recalled_at/by`, `version+1` | `MessageRepo::recall_locked_outboxed_in_tx` (any node) | PG primary | Durable; single tx with history/audit/outbox | Read Committed + explicit locks; `FOR UPDATE` row lock serializes all mutation kinds; `WHERE recalled_at IS NULL AND deleted_at IS NULL` final fence; version monotonic | PG replication (not evidenced in repo; single-primary assumed) | PG failover out of scope; row survives; state machine enforced in code, not CHECK |
| `event_outbox` row (`kind='recalled'`, `aggregate_version=MAX+1`, `seq`, `attempts`, `claimed_at`, `available_at`) | Outbox relay — **any** instance (SKIP LOCKED lease, 30 s) | PG | Durable queue | Lease + **attempts fencing** on `mark_published`/`mark_failed`; per-message aggregate precedence (`NOT EXISTS earlier unpublished`); per-subject seq assign-once (`COALESCE`) | Multi-instance claim, no coordination | Crashed worker leaves no permanent `processing` state: reclaim after lease expiry; backoff 1 s → 300 s cap, no dead-letter |
| NATS JetStream `im.room.{id}` + durable consumers (`aero-server`, bots, webhook dispatcher) | relay publisher (`publish_bytes_idempotent`, `event_id` as `Nats-Msg-Id`) | JetStream | Durable stream + durable cursors | at-least-once; per-subject seq stamped at publish; redelivery carries same seq | NATS cluster (not evidenced) | Consumer cursor durable; broker outage → rows stay pending, delivered on recovery with order preserved via `available_at, created_at` claim order |
| Per-subject seq counter `aero:seq:{subject}` | `SeqStore::next` (Redis `INCR`) | Redis | **Not durable** (no persistence assumption; keys intentionally TTL-less) | Atomic INCR, cluster-wide; gaps legal (crash may consume an unpublished value); negative saturation guard | Redis (not evidenced) | Fail-open: Redis down → `None` → unstamped event, never blocks delivery (documented contract) |
| `Hub` conns/rooms/watchers | process-local | memory | Ephemeral | Bounded mpsc per connection; disconnect-on-full (drop, never block) | **None** — per-process fan-out only | Reconnect + `changes_since` replay (see DS-2: currently **broken for recall**) |
| `AiContextStore` answer cache | bus chokepoint invalidation on `Edited/Deleted/Recalled` (`bus.rs:457`) | Redis | Cache | Best-effort; only when `state.ai.is_some()`; idempotent across instances | — | Fail-open (warn, never fail delivery) |
| `message_edits` row (pre-recall snapshot, `editor_id`=recaller) | recall tx | PG | Durable, no retention sweep (evidence stance) | Same tx | — | — |
| `audit_events` `message.recalled` (120-char digest) | recall tx | PG | Durable | Same tx | — | — |
| Blob GC queue (now-unreferenced attachments) | recall tx `enqueue_unreferenced_blobs_in_tx` | PG | Durable queue | delete-then-ack; live-ref check + cancel covers attach/delete race | — | `blob_gc_drain` timer, 60 s |
| `messages_partitioned` shadow | `backfill_messages_partition` (0148/0149/0158/0174 lineage) | PG | Cold, **not live** (cutover not in auto chain) | Mirror invariant documented in runbook | — | **DS-3: backfill projection omits recall columns — cutover would silently drop recall state** |
| Client `lastChangeSync` cursor | web SPA (per room) | JS memory | Ephemeral | Wall-clock based (`new Date().toISOString()`) | — | Replay on reconnect via `GET /api/rooms/:id/changes` |

**Stock wiring: verified.** Every live path (relay 250 ms loop in `background.rs`, bus listeners, webhook dispatcher consumer, blob GC, seq via `boot/services.rs:105,167` Redis `SeqStore`) is wired by the stock binary; the feature introduces no new store.

---

## 2. Findings (severity, evidence, trigger, impact, recovery, corrective pattern)

### DS-1 — HIGH · Async system edit re-enters the terminal recalled state; the version-based outbox suppression then *silently erases* the pending recall event

**Evidence.** `edit_locked_outboxed_in_tx` (`events.rs`) UPDATE WHERE is `id = $4 AND deleted_at IS NULL AND version = $5` — **no `recalled_at IS NULL`**. The user edit path is protected only incidentally (preflight `messages.rs:391` + client-held `expected_version`), but `edit_outboxed_system` (`unfurl_bot.rs:175-189`) re-reads `get_version` **after** the recall commit, so the version check passes and the UPDATE writes the pre-recall original blocks + preview card back into the recalled row, recomputing `searchable_text` → re-indexed into FTS. Then, if the recall's fast-path dispatch failed (NATS blip at commit; the slow path is the only delivery), `materialize_outbox_payload`'s Recalled arm (`outbox.rs:263-280`) compares `message.version > original_version` — the resurrection edit bumped version to V+1 — and **suppresses the pending `Recalled` outbox row** (`mark_published` without delivery). The suppression heuristic that correctly discards stale edits now discards the recall itself.

**Triggering failure.** `unfurl_bot` network fetch (HTTP fetch of link target, unbounded latency) completes after a recall; NATS unavailable at recall commit time (or any fast-path failure). Timeout-uncertain async side effect re-entering a one-shot terminal state.

**User impact.** Recalled message content returns to view (placeholder replaced by original text + preview), FTS/vector search re-indexes it, and — in the suppression case — **no client ever renders the placeholder at all**, for the entire room. Recall's core promise (content gone from all viewers) is violated silently; the metric `aero_messages_recalled_total` still counts a "successful" recall.

**Recovery.** Manual re-recall (author/admin), which then also un-suppresses nothing already lost; audit rows show `message.recalled` followed by `message.edited` — diagnosable but post-hoc.

**Corrective pattern.** State-machine fence at the storage layer, not the caller: add `AND recalled_at IS NULL` to the edit UPDATE WHERE (or an early `Ok(None)`/Conflict in `edit_locked_outboxed_in_tx` mirroring the `deleted_at` guard), so *every* editor — user, system, bot — is fenced. This is the same class of bug as unfenced state transitions: the guard must live in the one transaction that writes the row. Regression test: recall → `get_version` → `edit_outboxed_system` → assert no-op, version unchanged, blocks == placeholder (reproduced failing today by the testing reviewer).

### DS-2 — HIGH · Change-replay (resync) never delivers recalls — offline clients keep original content forever

**Evidence.** `changes_since` (`query.rs:177`) predicate: `GREATEST(edited_at, deleted_at) > $2`; the recall UPDATE sets `recalled_at` but **never bumps `edited_at`** (verified in `recall_locked_outboxed_in_tx`'s SET list). The client is ready — `applyChange` (`app.js:328`) routes `m.recalled_at` → `handleRecalled`, and `replayChanges` (`app.js:335-343`) calls `api.roomChanges` on reconnect — but the server never returns the row. Cursor advance happens before the reply (`app.js:337`), so the miss is **permanent**, not just delayed. This is a convergence gap on the partial-success path: live frame delivered to connected clients, durable cursor advanced, no other channel.

**Triggering failure.** Client offline (or WS dropped) at the moment the `Recalled` frame fans out; reconnects later; also any client whose Hub queue dropped the frame (disconnect-on-full).

**User impact.** The recalling user's message still shows its original content on every client that missed the live frame — exactly what recall is supposed to prevent. Violates the completion report's claim "all clients render the placeholder."

**Recovery.** None automatic. Manual re-recall triggers a *new* event, but `changes_since` still won't emit it.

**Corrective pattern.** Include recall in the mutation predicate: `GREATEST(edited_at, deleted_at, recalled_at) > $2` (or set `edited_at = recalled_at` at recall — simpler, keeps the existing expression index shape). Requires migration 0239: the 0125 expression index `(room_id, GREATEST(edited_at, deleted_at))` must be dropped/recreated for the new expression (or add `idx_messages_room_recalled`) + regression test (message + recall + `changes_since` returns it with `recalled()`). Note the design doc's `recalled_at`-as-order-key choice for `context.js` replay is unaffected.

### DS-3 — MEDIUM · Partition-cutover backfill omits recall columns (latent, cutover-time data loss)

**Evidence.** `backfill_messages_partition` was last reissued in 0174 with an explicit column list; 0238 adds `recalled_at`/`recalled_by` to the shadow + reconciles **existing** shadow rows, but never reissues the function. 0158's header documents exactly this bug class (silent fallback to column DEFAULT). Empirically demonstrated by the database architect on a throwaway DB: backfilled shadow rows get `recalled_at = NULL`.

**Triggering failure.** Any partition cutover run after 0238 (runbook Step C requires shadow + backfill + verify in the same change). Rows backfilled after 0238 are affected — 0238's reconcile only fixes rows present at migration time.

**User impact.** Post-cutover: "已撤回" badge disappears, the `recalled()`/edit guard (`recalled_at IS NULL`) makes the placeholder **editable again**, context replay breaks, `recalled_by` audit linkage lost.

**Recovery.** Re-run reconcile / re-backfill from live rows before serving cutover traffic.

**Corrective pattern.** Amend 0238 (undeployed branch) to reissue `backfill_messages_partition` with `recalled_at, recalled_by` in both INSERT and SELECT lists (mirror 0158/0174); extend runbook §5 verification (`shadow.recalled_at IS NULL WHERE live.recalled_at IS NOT NULL` must be 0). If 0238 ships as-is, 0239 before any cutover.

### DS-4 — MEDIUM · Rolling-deploy version skew: old binaries ack-drop `recalled` events and advance the durable cursor — silent permanent loss

**Evidence.** `run_bus_listener` poison policy (`bus.rs:513`): an undecodable payload is **ACK-dropped** (`BUS_POISON_DROPPED_TOTAL`), never nacked. A pre-recall binary receiving `kind:"recalled"` (unknown variant → decode error → poison path) drops it and acks. Durable cursor advances; no replay later. Producer is necessarily a new binary, so the hazard window is a mixed fleet (new producer + old consumer node), which is precisely the steady state of a rolling deploy.

**Triggering failure.** Any recall issued while ≥1 instance runs an old binary; clients connected to that instance never render the placeholder (they keep the original), and the AI answer-cache purge on that node never runs.

**User impact.** Per-instance silent loss window until the old node restarts. No metrics distinguish poison-drop of `recalled` from other poison.

**Recovery.** None automatic (cursor advanced). Deploy-ordering discipline: all binaries upgraded before recall is used in production.

**Corrective pattern.** This is a documented trade-off of the poison policy (poison must not block the queue). Two mitigations without changing the policy: (a) explicit release note + deploy gate (upgrade-all-before-use); (b) optionally count poison by discriminator to make the window observable (`BUS_POISON_DROPPED_TOTAL` label with kind when decodable-as-JSON). Acceptable as designed; must be documented, not assumed.

### DS-5 — LOW/MEDIUM · Cross-message room event order is not commit-ordered (fast-path claim overtakes older pending rows)

**Evidence.** Claim order is `available_at, created_at, id` (SKIP LOCKED) — but `dispatch_event_outbox_id` (post-commit fast path) claims a *new* row immediately, while an older row stuck in backoff (NATS outage) waits. So event B for a room can publish before event A. Per-message aggregate ordering is enforced (the `NOT EXISTS earlier unpublished` fence), and per-subject seq is monotonic, but **room-wide cross-message order is not a guarantee**. Self-healing property verified: a `Message` create event materializes from the *current* row (`outbox.rs:224-246`), so if the create is delayed past a recall, the delivered create already carries the placeholder — consistent end state, no client-side anomaly beyond arrival order.

**User impact.** Clients may observe frames out of commit order across different messages (e.g., a later message's create before an earlier message's recall). Clients keyed on seq reorder; seq is the contract. Low impact — but the contract should be stated, not assumed.

**Corrective pattern.** None required (documented contract: per-message aggregate + per-subject monotonic seq; no room-wide total order). If ever needed, publish only via the batch relay with commit-time ordering — out of scope.

### DS-6 — LOW · Clock assumptions: client wall-clock cursor, app-clock `recalled_at`, lease arithmetic

**Evidence.** (a) Client replay cursor is `new Date().toISOString()` (`app.js:526`) — a client clock rollback makes `since` sit in the future and mutations during the rollback window are skipped until the *next* reconnect; forward skew only causes harmless replays (applyChange idempotent). (b) `recalled_at = now_utc()` (app clock) is the ordering key for recall; no monotonic clock source (no HLC/Lamport) — cross-node ordering of recalls on *different* messages is already non-guaranteed (DS-5), so this is consistent. (c) Lease arithmetic (`lease_stale_before(now, lease)`, 30 s): a node clock rollback >30 s stalls claims (rows not yet "stale") until the clock catches up — latency only, because `mark_published`/`mark_failed` are fenced on `attempts` (a stale worker cannot overwrite a newer claim). No corruption path found.

**Corrective pattern.** None required beyond documentation; client cursor skew is bounded by the next reconnect and the (to-be-fixed) DS-2 replay.

### DS-7 — LOW · Recall is deliberately non-idempotent; 409 conflates "raced" with "already recalled" — retry-ambiguous under at-least-once client retries

**Evidence.** Double-submit → second returns 409 both for a concurrent race (`Conflict("message recall raced with another mutation")` from `recall_outboxed_authorized` returning `None`) and for an already-recalled state. REST route `POST /api/messages/:id/recall` has no `Idempotency-Key` handling (unlike message POST). A client that retries after a timeout-ambiguous failure gets 409 and cannot distinguish "another actor recalled it" from "your retry landed after your own success."

**Corrective pattern.** Document the 409→success mapping client-side for own-retries (safe: recall is terminal and content-equivalent), or add a recall `idempotency_key` column. Low priority; matches the plan's deliberate one-shot design.

### DS-8 — POSITIVE · Fencing/lease/at-least-once machinery is sound (verified, no action)

- `mark_published`/`mark_failed` fenced on `attempts` + `claimed_at IS NOT NULL` + `published_at IS NULL` — stale reclaims cannot overwrite newer claims; superseded-lease paths return `false` and are handled.
- `assign_seq_if_absent` uses `COALESCE(seq, $2)` — a re-claimed row re-publishes with the **same persisted seq**, so NATS dedup-window expiry (duplicate delivery beyond the broker window) is still dedupable client-side by seq.
- Crash windows: commit→fast-dispatch (relay picks up within lease); publish→`mark_published` (re-publish with same `event_id` Msg-Id; duplicate beyond dedup window carries same seq). Both bounded, both dedup-covered.
- Pending-row materialization rebuilds from current row state — delayed recall after NATS outage carries the current placeholder, and Notify recipient lists are re-validated against current membership at publish time.
- Consumer-side re-authorization (`bus.rs` reads membership per event) protects delayed events from reaching departed members.

---

## 3. Scenario table (partition, crash, retry, clock, stale cache, dependency outage, recovery)

| # | Scenario | Behavior today | Consequence | Recovery |
|---|---|---|---|---|
| 1 | Relay/instance crash after recall commit, before fast dispatch | Row pending, lease 30 s; another instance (or restart) claims | Recalled delivered late (≤ lease + poll) | Automatic |
| 2 | Crash after NATS publish, before `mark_published` | Re-claim → re-publish same `event_id` (Msg-Id dedup) + same persisted seq | At-most one extra delivery; client seq-dedup | Automatic |
| 3 | NATS broker outage at recall commit | Fast path fails; relay backoff 1 s→300 s; row stays pending | Delivery delayed up to outage duration; materialize rebuilds current placeholder; per-subject order preserved via `available_at, created_at` | Automatic on recovery |
| 4 | PG outage (recall tx or claim) | Tx fails → 500, no partial state (single tx); relay batch error → warn + retry | No recall applied; no ghost rows | Automatic |
| 5 | Redis outage (seq) | `next()` → None → unstamped publish (fail-open contract) | Gap/unstamped event; clients pass through; recall frame still delivered; dedup degrades to idempotent applyChange | Automatic |
| 6 | Node clock rollback > 30 s (relay) | Claims stall until clock catches up; fencing prevents double-publish | Delivery latency only; no corruption | Automatic |
| 7 | Client clock rollback (replay cursor) | `since` in future → mutations in rollback window skipped until next reconnect; recall missed (compounded by DS-2) | Offline client keeps original content longer | Next reconnect (post DS-2 fix) |
| 8 | Mixed-version fleet (rolling deploy) | Old node ack-drops `recalled`; durable cursor advances | Its clients never render placeholder; cache purge missed; **permanent** | None — deploy ordering (DS-4) |
| 9 | Unfurl slow fetch + recall commit + NATS blip | System edit resurrects content; pending Recalled suppressed by version heuristic | Content back in view for whole room; recall event never delivered | Manual re-recall (DS-1) |
| 10 | Client offline during recall fan-out | `changes_since` never returns the row | Client keeps original content indefinitely | None (DS-2) |
| 11 | Concurrent recall vs delete | Row lock serializes; delete-first → recall 409 "deleted"; recall-first → delete proceeds, Recalled suppressed at materialize (row gone), clients see Deleted only | Consistent terminal state either way | Automatic |
| 12 | Concurrent double recall | Row lock + `WHERE recalled_at IS NULL`; exactly one wins, one 409; exactly one outbox row | Correct, but 409 retry-ambiguous (DS-7) | Client maps 409→success |
| 13 | Duplicate delivery of Recalled frame (NATS redelivery) | Same seq; client dedup; applyChange idempotent | No visible effect | Automatic |
| 14 | Lease expiry mid-publish (worker A slow) | B claims, publishes, marks published; A's `mark_published` fenced on attempts → `false` → "superseded lease" | No double publish | Automatic |
| 15 | Partition cutover with stale backfill | `recalled_at/by` dropped to DEFAULT in shadow rows | Badge gone, placeholder editable, audit linkage lost (DS-3) | Re-backfill before cutover |
| 16 | Stale AI answer cache after recall | `Recalled` in purge set at bus chokepoint (per node, idempotent) | Cache invalidated; best-effort (warn on failure) | Automatic |

---

## 4. Stated guarantees, unsupported topologies, validation tests, residual risks

### Guarantees the feature does (and should) state

1. **Atomicity**: placeholder write + history snapshot + audit + blob enqueue + outbox row commit in one PG transaction; no partial recall.
2. **One-shot transition**: recall is terminal and non-idempotent; `WHERE recalled_at IS NULL AND deleted_at IS NULL` final fence under row lock; stable failure ordering 404 → 403 → 409(deleted) → 409(recalled) → 403(role); no existence oracle across tenants.
3. **Per-message aggregate ordering**: create < edit < recall < delete enforced by aggregate-version precedence in claims + row-lock serialization of writers; `Recalled` outbox row cannot overtake its own create.
4. **At-least-once delivery with dedup**: stable `event_id` as NATS Msg-Id + per-subject seq stamped at publish, persisted across re-claims; redelivery and post-dedup-window duplicates carry the same seq.
5. **Lease-based multi-instance relay**: SKIP LOCKED + 30 s lease + attempts fencing; no permanent processing state; failed rows re-park with backoff (1 s→300 s), no dead-letter.
6. **Fail-open boundaries preserved**: Redis seq outage → unstamped (never blocks); AI-cache invalidation best-effort; blob GC delete-then-ack. The one fail-closed-ish trade-off is bus poison ack-drop (DS-4).
7. **Stale-event suppression** (mirror Edited): delayed Recalled suppressed when a later mutation superseded it — **but this is precisely the heuristic DS-1 abuses**; it must be paired with a storage-level terminal-state fence.

### Unsupported topologies / non-guarantees (must be documented, not assumed)

- **Mixed-version fleets** are unsafe for recall use (DS-4): old binaries silently ack-drop the new event kind.
- **Room-wide (cross-message) commit ordering**: only per-message aggregate + per-subject monotonic seq are guaranteed (DS-5); the fast path can overtake backoff'd rows.
- **Exactly-once to external consumers**: webhook/processor copies of pre-recall content are irrevocable (send-time delivery); recall is a display-level control, not erasure.
- **Multi-primary PG, Redis persistence for seq** (gaps legal by design), **cross-node Hub state** (reconnect replay is the only bridge — currently broken for recall, DS-2).
- **Partition cutover automation** — not in the auto chain, and the backfill function is currently wrong (DS-3).

### Validation tests

**Existing and passing** (per completion evidence + prior reviewers' re-runs): permission matrix (pure + storage + im-core), failure-ordering, cross-tenant isolation, delete-after-recall, migration schema/CHECK, WS frame contract, event serde round-trip, backfill-exclusion of placeholders, AI-cache purge, relay suppression arms for Edited/Deleted (but **not** Recalled).

**Missing — blocking (write failing-test-first):**
1. `recall → get_version → edit_outboxed_system → no-op` (DS-1; reproduced failing).
2. `changes_since` returns recalled messages with `recalled()` (DS-2; reproduced failing) + index change in 0239.
3. Unit test for the `materialize_outbox_payload` Recalled arm (suppressed when version advanced; delivered with current placeholder otherwise).
4. Concurrent double-recall race (exactly one Some, one Conflict, one outbox row).
5. Backfill parity check for recall columns (DS-3 gate query).

**Not executable here**: real-browser E2E; rolling-deploy skew (DS-4) is a deployment discipline, not a unit test — assert it in the release note.

### Residual risks

- **DS-1/DS-2 are live defects** in the current tree; both violate the feature's core promise ("content removed from all viewers") and both were reproduced with failing tests by the testing reviewer. Neither is visible to the happy-path suite.
- Unbounded recall window (product decision open) — a bounded window would change the permission matrix and add a CHECK; unbounded maximizes admin-compromise blast radius (no alert on mass recall; `aero_messages_recalled_total` exists, no rate alert).
- Pre-recall content remains member-readable via history route + audit digest (compliance Finding 1/4) — a distributed-systems *and* privacy property that no code change in this feature addresses.
- `recalled_by`/`editor_id` not in GDPR erasure set (aligns with audit-retention stance; needs an explicit decision record).
- Client replay cursor is wall-clock; post-fix, a rollback window still delays convergence until the next reconnect (bounded, acceptable).

---

*This review is an engineering assessment of consistency, ordering, atomicity, idempotency, failover, and clock behavior; it is not a security audit or legal advice. All SQL/SQLX predicates quoted were verified against source on this tree; no live-DB replay was re-run here (the testing reviewer reproduced the two blocking defects with failing tests on the same tree).*
