All claims verified against the shipped tree (`f20e4d1` + working tree). `prompts/README.md` does not exist in this repository; I applied the security invariants from `AGENTS.md` (§4) and the adversarial-review focus directly. Every anchor cited below was read in source, and the CI authz lint re-ran green locally (7/7).

---

# Security Review — Message Recall Time Window (v3 as-built)

**Scope**: `AERO_RECALL_WINDOW_SECS` plumbing, two-layer window enforcement (preflight + row-locked tx fence), 409 contract, metric, web mapping, and the recall surface they gate (`POST /api/messages/:id/recall`, WS `recall_message`, migration 0238 prerequisites).

## 1. Assets, trust boundaries, attacker capabilities, entry points

**Assets touched by the feature**: message rows (`blocks` placeholder, `recalled_at/by`, `version`), `message_edits` evidence snapshot, `event_outbox` (kind `recalled`), audit (`message.recalled`), blob GC queue, workspace rate budget, `aero_messages_recall_expired_total` counter, `AERO_RECALL_WINDOW_SECS` config.

**Trust boundaries**:

| Boundary | Who crosses it | What the feature exposes |
|---|---|---|
| Internet → gateway | Unauthenticated / any `AuthUser` | 404 vs 403 vs 409 vs 429 envelopes only; no state string to non-privileged callers |
| Workspace tenant edge | Member of workspace A probing message in workspace B | `assert_room_access` → 403 before any state check (both transports) |
| Room membership edge | Member of room probing a message they don't own | 403 from role gate *before* the window check — window state never revealed |
| Author edge | Author of an expired message | Only this actor ever sees `"conflict: recall window expired"` |
| Admin/owner edge | Room owner/admin | Exempt from the window (moderation path); the window string is unreachable for them |
| Ops edge | Deployer | Config knob: invalid → silent fallback to 86400s (restrictive direction) + boot log |

**Attacker capabilities considered**: (a) any authenticated user in any workspace; (b) in-room member; (c) author of an expired message; (d) concurrent actor racing a demotion/admin recall; (e) operator misconfiguration; (f) deploy-time availability. No new capability was introduced: the feature adds no unauthenticated surface, no outbound network, no headers, no crypto, no token handling, no proxy trust.

**Entry points**: REST `POST /api/messages/:id/recall` (`routes.rs:133`, `AuthUser`, per-client HTTP limiter at 20 rps default via `rate_limit::layer`); WS `ClientFrame::RecallMessage` (`frame.rs:163`, no per-frame limiter before preflight — see F1); the storage fence `recall_outboxed_authorized` as the commit-time authority; migration 0238 as the deploy-time surface.

## 2. Findings (severity-ordered)

### F1 — Low: WS preflight path is unthrottled; the documented "≤20 rps bounded" bound applies to REST only

- **Evidence**: `frame.rs:163-165` runs `assert_message_recall_preflight` *before* `check_ws_rate_room`; the only limiter before the preflight is the HTTP `rate_limit::layer` (`boot/serve.rs:129`), which does not apply to frames after the WS upgrade. The frame loop has no per-connection token bucket (verified — no frame-level throttle in `ws_impl/frame.rs`). `docs/recall-window.md` states "限流前的作者探测可使该计数器小幅增长（每参与者 ≤20 rps，有界）".
- **Exploit preconditions/steps**: authenticated author of an expired message in any room; open one WS connection (handshake is HTTP-rate-limited to 20/s, but each connection then pumps frames at server processing speed); send `{"type":"recall_message","id":<own expired message>}` repeatedly. Each frame: 1 PK `get` + `assert_room_access` (3–5 reads) + `role_of` (1 PK read) + `aero_messages_recall_expired_total++` — all *before* the workspace gate, which is never charged (preflight 409s first), so it never trips. Counter grows without bound; ~100–300 preflight batches/sec per connection × 20 connections/sec.
- **Impact**: unbounded inflation of the metric the design sells as the US4 tuning signal (alert integrity), plus unthrottled cheap read amplification. Same *shape* as the pre-existing edit preflight (identical ordering), so not a new vulnerability class — but the metric makes the recall variant observable and is the one genuinely new amplification artifact.
- **Remediation**: (a) a per-connection frame token bucket at the top of the frame handler covering preflight-bearing frames (fixes recall *and* the identical edit preflight); or (b) if the preflight must stay ungated, at minimum correct the doc bound to "REST ≤20 rps; WS bounded only by connection count/frame processing".
- **Regression test**: WS harness sending N rapid `recall_message` frames for an expired own message; assert the counter increments at most `budget` per window once a gate exists, or (doc-only fix) assert the doc claim is absent.

### F2 — Low: `message_edits` evidence retention means recall ≠ content removal for room members; the rollback runbook overstates irrecoverability

- **Evidence**: `authorization.rs:312-317` snapshots the full pre-recall body into `message_edits` (redaction removes *byte refs* only — `redact_blocks_for_recall_snapshot`, `message/mod.rs:189-204`; text blocks and voice transcripts survive verbatim). `GET /api/messages/:id/history` (`message_history.rs`) serves it to **any room member** (`list_for_message` joins live messages where `deleted_at IS NULL` — a recalled-but-not-deleted message still matches). The DB architect's rollback section ("original content is irrecoverable at the app level") is accurate for the *live message* but not for member-visible history.
- **Exploit preconditions/steps**: none required — this is a documented-intent discrepancy, not a bypass. A member who recalls (or an admin recalling for abuse remediation) expecting removal from room view will find the full text readable via the history route.
- **Impact**: sensitive-data handling surprise for operators choosing recall as a remediation tool; search/RAG/embedding are correctly cleaned (`searchable_text=''`, `embedding=NULL`, blobs GC'd), so the residual surface is the history route + audit digest + backups. Pre-existing recall semantics (0238), unchanged by the window feature — but the window feature's docs are the place to state it.
- **Remediation**: add the caveat to `docs/recall-window.md` 非目标 (evidence trail is member-visible by design); if removal-from-members is ever desired, gate the history route on `recalled_at` (product decision, not a code defect today).
- **Regression test**: pin the semantics — recalled message → history route returns the redacted snapshot (text preserved, byte refs absent); deleted message → history returns empty (already true; no committed pin found in `message_history.rs`/`message_edit.rs`).

### F3 — Low: migration 0238 rebuilds `idx_messages_room_mutated` non-concurrently (deploy-time availability; prerequisite migration, not in this diff)

- **Evidence**: `migrations/0238_message_recall.sql` — `DROP INDEX IF EXISTS` then `CREATE INDEX IF NOT EXISTS` (no `CONCURRENTLY`) plus a full `UPDATE messages_partitioned … FROM messages` backfill join; during the gap `changes_since` (reconnect backfill) has no index (seq scan per room).
- **Exploit preconditions/steps**: operator deploys with a large `messages` table (dev DB is 746 rows — no current impact). `CREATE INDEX` takes a `SHARE` lock blocking message INSERT/UPDATE/DELETE for the build duration.
- **Impact**: write stall + slow reconnects at deploy time — a self-inflicted availability window.
- **Remediation**: `CREATE INDEX CONCURRENTLY` under a new name, `DROP INDEX` the old one after (with the documented concurrent-build retry loop); batch the shadow `UPDATE` in id-ordered chunks (the reissued `backfill_messages_partition` already does this — use it instead of the single join).
- **Regression test**: none practical at CI scale; add a runbook/deploy-checklist item (validation query `EXPLAIN` a `changes_since` query → index scan).

### F4 — Info: 404-vs-403 existence oracle on arbitrary message UUIDs (pre-existing pattern, inherited by both recall entry points)

- **Evidence**: `assert_message_recall_preflight` and `recall_message` both call `messages.get(id)` (→ `NotFound("message {id}")`) *before* `assert_room_access` (→ `Forbidden`); identical in `editable_message`. Any authenticated user probing `/api/messages/<uuid>/recall`: exists-but-foreign → 403; absent → 404.
- **Exploit preconditions/steps**: an outside attacker (or former member) with a *known* UUID confirms the message exists. UUIDs are v4/unguessable — enumeration is infeasible; the design's "no existence oracle" claim is overstated (state is protected, existence is not).
- **Impact**: minor cross-tenant existence disclosure, repo-wide (edit/delete identical), not recall-specific.
- **Remediation**: document the accepted contract (Slack-style: known-resource 403 vs unknown 404); do **not** unify to 404 — that would change the whole repo's error contract for a non-exploitable oracle. No code change recommended.
- **Regression test**: existing leak tests already pin the 403-no-window-string contract; add nothing.

### F5 — Info: window boundary error bound = inter-instance clock skew

- **Evidence**: `created_at` app-minted at insert (`crud.rs:71`); fence compares with the recaller's instance `now_utc()` (`authorization.rs:261-269`). Default 86400s dwarfs NTP skew; the 1s hermeticity gate and any tiny operator window inherit the skew as boundary error.
- **Impact**: correctness preserved (fence atomic under the row lock); only the boundary instant is fuzzy. An author cannot choose their serving instance, so no exploitable directionality — at most a ±skew early-409 (annoyance) or late-allow (bounded by skew) on small windows.
- **Remediation**: document "window ≫ configured clock-skew bound" in `docs/recall-window.md` (the DB architect's F2 recommendation). No code change.

### F6 — Info: config fail direction is safe; preflight duplicates the authority chain (accepted costs)

- `parse_recall_window` (`messages.rs:45`): garbage/negative/overflow → 86400s default — a typo fails **toward restriction**, never toward unlimited; only the exact string `"0"` (trim-tolerant) disables the window; boot log distinguishes fallback from intentional `0` (`orig.rs:316-323`). `time::Duration::seconds(i64::MAX)` is representable and compares safely (no panic path; `now − created_at` spans at most years).
- Preflight costs ~5–7 PK-indexed reads before the rate gate (`messages.rs:455-527`); this is the deliberate gate-S1 fairness trade (doomed attempts never burn workspace budget) — verified in both transports, and the workspace charged is the message's own (resolved from the row, not client input), so no cross-workspace budget drain is possible.

## 3. Abuse-case table

| Abuse case | Reachable? | Path & outcome | Verdict |
|---|---|---|---|
| **Identity spoofing** (`recalled_by` / actor) | No | Actor comes only from `AuthUser` (JWT/PAT) on REST and the authenticated WS connection; no client-supplied actor anywhere in the path | Not exploitable — verified |
| **Replay** of a captured recall request | No amplification | Second application hits preflight `recalled_at` → 409 "already recalled"; tx fence `WHERE recalled_at IS NULL` makes double-apply impossible; no auto-retry anywhere; convergent-silent on the web layer | Fenced — verified (pre-existing `concurrent_double_recall_has_exactly_one_winner`) |
| **Cross-tenant access** (workspace A user recalls workspace B message) | No | `get(id)` → `assert_room_access` (workspace + room membership + deactivation + 2FA gates) → 403 before any state check; IDOR/oracle pinned by leak tests + `authz_lint` 7/7 (re-ran locally) | Blocked — verified |
| **Window-state oracle** (member probes an expired message) | No | Role gate precedes window check in both layers; member gets 403, never the window string (4 pinned tests: storage `recall_window_no_leak_to_member`, service `_no_leak_to_member_service`, precedence tests, live E2E member-probe) | Blocked — verified |
| **Existence oracle** (outside actor probes known UUID) | Yes (inherited) | 404 (absent) vs 403 (exists-foreign); repo-wide edit/delete pattern; UUID unguessable | Accepted risk — F4 |
| **Proxy/header forgery** | No | Feature adds no header handling, no forwarded-IP trust, no outbound fetch | Not applicable — verified |
| **Resource exhaustion — metric inflation** via WS preflight pump | Yes | Author of an expired message pumps `recall_message` frames pre-rate-gate; unbounded counter growth + cheap PK-read amplification | Low — F1 |
| **Resource exhaustion — mutation** | No | `check_ws_rate_room` gates the mutation in both transports after preflight (Redis-failure fail-open is pre-existing documented gate behavior) | Gated — verified |
| **Resource exhaustion — deploy-time** | Conditional | 0238 non-concurrent index rebuild stalls writes on large tables | Low — F3 |
| **Sensitive-data leakage — recall content** | By design | Live row/search/embedding cleaned; pre-recall text readable by room members via history route (evidence trail); audit stores 120-char digest; attachments GC'd (delete-then-ack, in-tx enqueue) | Documented intent — F2 |
| **Sensitive-data leakage — error strings** | No | 409 body contains only the fixed window string; NotFound echoes the caller-supplied UUID; toast renders via `textContent` (no XSS); boot log contains no secrets/content | Not exploitable — verified |
| **SQL injection** | No | UUIDs parsed as typed ids (invalid → 400); all queries parameterized; window is a Rust `Duration`, never interpolated into SQL | Not exploitable — verified |
| **Race — author vs admin recall / mid-recall demotion** | No | Both check + apply under the message `FOR UPDATE` lock and the `room_members FOR UPDATE` role re-check; `tokio::join!` race test proves exactly one winner; edit-after-recall fenced by `recalled_at IS NULL AND version = $5` | Fenced — verified (F7 gap from QA: no dedicated demotion-race test — pre-existing gap pattern, shared with send) |

## 4. Positive controls verified, residual risks, prioritized validation plan

**Positive controls verified in source** (each independently re-checked, not taken from the design):

1. **Canonical guard everywhere**: both entry points route through `assert_room_access(participant, room)` (participant-first) and the storage fence through `lock_effective_message_write_access` + `recall_role_allowed_in_tx` (`FOR UPDATE` on `room_members`) — role re-checked under lock, demotion cannot slip through; lock order (workspace → rooms → membership → message) matches edit/delete/send — no deadlock inversion.
2. **Window fence atomic by construction**: evaluated on the `FOR UPDATE`-locked snapshot (`lock_message_in_tx` selects `created_at`), author-only (`sender_id == actor`), appended last after role/deleted/recalled gates; the UPDATE's `WHERE id AND recalled_at IS NULL AND deleted_at IS NULL` unchanged; edit-after-recall closed by the version bump.
3. **Oracle-safe 409**: window string author-only; precedence pinned (deleted → recalled → role → window in service; role → state → window in storage, pre-existing order preserved); exact wire body pinned by `error.rs:68`; prefix-tolerant web discriminator.
4. **Tenant-fair rate charging**: workspace resolved from the message row, never client input; doomed attempts (403/409) never charge the budget — gate S1 holds in both transports.
5. **Content sanitization**: `searchable_text=''`, `embedding=NULL`, placeholder-only broadcast (`RoomEvent::Recalled` carries no pre-recall content), blob GC enqueued in-tx, audit digest truncated.
6. **Metric choke point**: single emit site inside the preflight, after the role gate (member/non-member probes never increment), both transports share it.
7. **Config safety**: silent fallback fails restrictive; boot log distinguishes `0` from fallback; `0` = unlimited honored consistently in both layers (storage `ZERO` tests).
8. **CI gates**: `authz_lint` 7/7 re-ran green locally (recall route covered); `check/clippy/test/web` gates were independently re-executed green by the QA reviewer on the final tree.

**Residual risks** (accepted, none blocking): F1 WS preflight pump (metric inflation + cheap reads, same class as pre-existing edit preflight); F2 evidence-retention semantics (documented intent, needs doc caveat); F3 deploy-time index rebuild (needs runbook entry, only at scale); F4 existence oracle (repo-wide contract, unguessable ids); F5 clock-skew boundary fuzz (document "window ≫ skew"); QA F7 (no demotion-race test — pre-existing pattern, recommend mirroring `send_rechecks_admin_role_after_waiting_for_demotion` for recall).

**Prioritized validation plan**:

| # | Item | Priority | Owner |
|---|---|---|---|
| 1 | Per-connection WS frame budget before preflight-bearing frames (recall + edit), or correct the ≤20 rps doc claim | High (only actionable security gap) | Server |
| 2 | Add the member-visible evidence-trail caveat to `docs/recall-window.md` 非目标 | Medium | Docs |
| 3 | Pin history-route semantics test (recalled → redacted snapshot returned; deleted → empty) | Medium | Server/storage |
| 4 | Recall demotion-race test (`tokio::join!` recall vs role demotion → Forbidden, zero writes) | Medium | Storage |
| 5 | 0238 concurrent index rebuild + batched shadow backfill; runbook validation query | Low (deploy-time, scale-gated) | Ops/DB |
| 6 | Clock-skew bound note in `docs/recall-window.md` | Low | Docs |

**Bottom line**: the window feature adds no exploitable privilege boundary, tenant-isolation hole, injection, replay, or leakage vector — the authority chain is duplicated with a lock-atomic fence, the error contract is oracle-safe for all non-author actors, and the config fails restrictive. The two actionable items are the WS preflight pump (F1, bounded in kind by the pre-existing edit pattern but with a new observable counter) and the evidence-retention documentation gap (F2). No blocking findings; F1–F3 warrant hardening, F4–F6 are documented accepted risks.
