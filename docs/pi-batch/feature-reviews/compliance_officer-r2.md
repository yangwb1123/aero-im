# Compliance Review (Round 2) — Message Recall (消息撤回)

- **Date**: 2026-08-06 (round 2; supersedes `compliance_officer.md`, which is preserved)
- **Priming**: `prompts/README.md` does not exist in this repo (`/home/u1/aero-im/prompts/` absent); applied the shared rules at `/home/u1/ai-batch-runner/prompts/README.md`. Implemented controls are not evidence of certification.
- **Method**: re-derived every material claim from current source at the 0238 revision (not from prior reports): `migrations/0238_message_recall.sql`, `aero-storage/src/message/authorization.rs` (recall tx), `crud.rs` (`enqueue_unreferenced_blobs_in_tx`), `blob.rs` (`has_live_references`), `query.rs` (`changes_since`), `participant.rs` (erasure + deferred sweep), `message_history.rs`, `webhooks.rs`, `metrics.rs`, `bus.rs`, `db.rs`, `bin/boot/persistence.rs`, `aero-cli/src/main.rs`. Cross-checked against the round-2 security engineer and database architect reviews (their findings re-verified independently where load-bearing for compliance).
- **This round's gate evidence** (implementer artifact, not re-run by this reviewer): check/clippy green, lib tests 2158/0, web tests 82/0, authz_lint 6/6, fresh-DB replay of 238 migrations + 594 ignored tests, storage recall 9/9, im-core 28/28, backend-quality 0 violations, `COMPLETION: OK`.

---

## 1. Applicable-scope statement

**In scope.** The recall subsystem: REST `POST /api/messages/:id/recall` + WS `recall_message`, the transactional recall (migration 0238), `Recalled` room-event broadcast, client placeholder rendering, and the data lifecycle it touches: `messages` rows, `message_edits` snapshots, `audit_log`, `event_outbox`/NATS, embeddings/search, attachment blob GC, webhook/bot consumers, GDPR erasure paths, and the member-visible history route.

**Unknown (marked per instructions — the evidence does not establish them):**
- **Jurisdiction**: unknown. No deployment jurisdiction declared anywhere in the evidence.
- **Data classification**: unknown. No classification of recalled content (employee comms, customer PII, etc.) is asserted.
- **Framework applicability**: unknown. `participant.rs` cites GDPR Art. 17(3)(e) in a code comment (legal-hold exemption) — **evidence of GDPR-informed design, not a legal determination** that GDPR applies to this deployment. SOC 2, ISO 27001, HIPAA, LGPD, etc.: not referenced anywhere in the evidence — excluded from scoring, listed as not established.

**Excluded (unchanged from base system, carried not re-assessed):** no new vendors, no new cryptographic material, no new storage tier, no new keys. Encryption/key-management obligations are inherited and out of recall's change radius.

**Load-bearing characterization (unchanged from round 1, re-verified):** recall is a **display-level placeholder operation, not erasure**. Pre-recall content survives in `message_edits` (member-visible via `GET /api/messages/:id/history`, `message_history.rs:64` gates with `assert_room_access` only), in the `message.recalled` audit digest (first 120 chars, `authorization.rs:278`), and irrevocably in webhook-consumer and client caches. This is deliberate and Slack-like, but it means **recall cannot satisfy an erasure request**, and round 2 adds a new integrity defect: for **attachment bytes**, the retained snapshot is *not* intact (Finding 1) — the two halves of the story now contradict each other.

---

## 2. Control matrix

Status: ✅ implemented & verified · ⚠️ implemented with gap · ❌ gap/open decision · ➖ carried/inherited.

| # | Requirement | Status | Repository evidence | Process evidence | Gap | Owner | Validation |
|---|---|---|---|---|---|---|---|
| 2.1 | Least privilege: author or owner/admin only | ✅ | `recall_authorized` pure fn (im-core `messages.rs:22`); commit-time `recall_role_allowed_in_tx` re-check under `FOR UPDATE` on `room_members` (`authorization.rs`) | Table-driven matrix tests (`recall_permission_matrix_author_admin_owner_member`, im-core 28/28 green) | None found | Backend eng | Re-run storage/im-core ignored suites |
| 2.2 | Tenant isolation: `assert_room_access` before any state check | ✅ | Service order get→`assert_room_access`→deleted→recalled→role (`messages.rs`); commit-time `aero_effective_room_access` re-check in tx | `recall_cannot_cross_workspace_boundaries`, `recall_cross_room_member_is_forbidden_without_state_leak` | **Plan wording "防存在性 oracle" overstates: 404 (unknown) vs 403 (exists-but-inaccessible) leaks existence to cross-tenant callers** (pre-existing on edit/delete; ULIDs unguessable; state is protected) | Backend eng | Doc fix + optional probe test (security F2) |
| 2.3 | TOCTOU/race resistance | ✅ | Message `FOR UPDATE`, membership `FOR UPDATE`, identity re-validation, final `WHERE recalled_at IS NULL AND deleted_at IS NULL` fence; `version+1` | `concurrent_double_recall_has_exactly_one_winner` (9/9 storage recall green) | None found | Backend eng | Ignored db_tests |
| 2.4 | REST + WS converge on one invariant | ✅ | Both paths → `ImService::recall_message`; frame contract test `recalled_frame_shape_carries_placeholder_message`; 409→success client mapping | ws/api/render tests (82/0 web) | None found | Backend eng | `node --test web/` |
| 2.5 | Audit trail, same tx | ✅ | `audit_log` `message.recalled` row (workspace, actor, message id, 120-char digest) in the recall tx (`authorization.rs:336-345`) | `author_recall_replaces_content_and_records_audit_in_one_tx` | **Digest retains 120 chars of original content in plaintext; audit access/retention policy not evidenced** | Platform/legal | Audit access test + retention policy (Finding 5) |
| 2.6 | Data minimization on live surfaces | ✅ | Placeholder `blocks`, `searchable_text=''`, `embedding=NULL`; embedding-backfill excludes (predicate `searchable_text <> ''`) — re-verified no gap | Recall tx test asserts placeholder + cleared fields | None found | Backend eng | Code inspection |
| 2.7 | Attachment byte disposal (minimization half) | ✅ (mechanism) | `enqueue_unreferenced_blobs_in_tx` (dedup-aware, `FOR UPDATE` on blobs, containment predicate) + drain `has_live_references` | `blob_gc_drain` delete-then-ack timer | **Both reference checks scan `messages` only — `message_edits` snapshot (same tx) still references the bytes when they are deleted within ~60 s** | Backend eng | **Finding 1 (NEW, Medium)** |
| 2.8 | Evidence/retention integrity of the snapshot | ❌ | `message_edits` stores prior `blocks` incl. `blob_id`s; history route serves them member-visible | — | **Recalled attachment bytes are destroyed while the member-visible history snapshot still points at them** | Backend eng | **Finding 1 (NEW, Medium)** |
| 2.9 | Retention/deletion lifecycle incl. GDPR erasure | ⚠️ | Primary erasure anonymizes recalled rows + their `message_edits` (participant.rs); `recalled_message_can_still_be_deleted` | — | **Post-legal-hold deferred sweep skips recalled rows** (`participant.rs:644` predicate `searchable_text <> '' OR embedding IS NOT NULL`; recall clears both) — pre-recall snapshot survives indefinitely after hold release; **`recalled_by` not in erasure set** (consistent with `sender_id` tombstone stance but undocumented) | Platform eng | **Finding 2 (open from r1, re-verified)**; DB-architect F4 |
| 2.10 | Offline/reconnect convergence of recall | ⚠️ | `changes_since` uses `GREATEST(edited_at, deleted_at, recalled_at)` + reissued index (fix verified); cursor hygiene fix in `ws.js` | `changes_since_delivers_recalls` green | **`changes_since` clamped to 200, single page, client advances cursor before reply and never loops → >200 mutations in a reconnect window silently skip the recall; offline member keeps pre-recall content until reload** | Backend eng | **Finding 3 (NEW, Low)** (DB-architect F2) |
| 2.11 | Recall window / policy | ❌ | Doc: "no time limit on recall window — product decision left open" | — | No policy, no configurable window, no recorded rationale | Product owner | **Finding 4 (open from r1)** |
| 2.12 | User notice / disclosure | ❌ | Placeholder `[此消息已被撤回]`; recall terminal (edit/react/reply suppressed in render.js) | — | No user-facing notice that pre-recall content remains in member-visible history, audit, and processor copies; UI wording "撤回" overstates the effect | Product + legal | **Finding 6 (open from r1)** |
| 2.13 | Abuse detection / observability | ⚠️ | `aero_messages_recalled_total` counter + histogram op `recall` | — | **No alert/runbook on recall rate per actor/workspace** (admin mass-recall uncapped, unthrottled on both entry points — WS has no `check_ws_rate_room`) | SRE | **Finding 7 (open from r1)**; security F3 |
| 2.14 | Third-party (webhook/processor) data flow | ⚠️ | `webhooks.rs:387` maps `Recalled` → `"recalled"` (placeholder body at recall time); bots act only on `Message` | — | **Processor register must record that consumers received the original at send time and cannot be un-sent; client-rendered copies irrevocable** | Privacy/legal | Processor register (open from r1) |
| 2.15 | Migration/rollout reliability of the control | ⚠️ | 0238 additive (2 nullable cols + CHECK extension + shadow reconcile + index/backfill reissue); deploy discipline DS-4 (upgrade all nodes first) | Fresh-DB replay green (238 migrations) | **Migrations run on a pool whose `after_connect` sets `statement_timeout=10 s` (`db.rs:31`, `boot/persistence.rs:33`) — 0238's `CREATE INDEX` can abort the chain on production-sized `messages`; green gates only prove empty-DB replay** | Platform eng | **Finding 5 (NEW, Low/scale-conditional)** (DB-architect F1) |
| 2.16 | Encryption / key management | ➖ | No new keys, no new storage tier | — | Re-assess at deployment level only | Infosec | Existing crypto review |

---

## 3. Findings

Sorted by severity; evidence is from current source unless noted. `r1` = carried forward from round 1 (re-verified open this round); `NEW` = introduced by round-2 reviews and independently verified here.

### F1 — MEDIUM (NEW) · Recalled attachment bytes are GC'd while the member-visible history snapshot still references them

- **Verified evidence**: recall tx (`authorization.rs` ~300) inserts the original `blocks` (incl. `blob_id`s) into `message_edits`, then calls `enqueue_unreferenced_blobs_in_tx` (`crud.rs:306-340`) whose live-reference check scans **`messages` only** — at that point the live row already carries the placeholder, so the blobs are enqueued. The drain-time check `has_live_references` (`blob.rs:328`) also scans `messages`/emoji/integration ledger only — **not `message_edits`**. Bytes are deleted within ~60 s. `GET /api/messages/:id/history` (`message_history.rs:64` `assert_room_access` → `list_for_message`) is member-accessible for live-but-recalled rows (`deleted_at IS NULL`) and serves the snapshot.
- **Business/regulatory risk**: (a) the feature's own evidence promise ("snapshot is evidence, not a loss") is broken for attachments — the retained audit/evidence trail is incomplete and silently divergent (text retained, bytes gone); (b) room members following the documented history path see broken attachment metadata; (c) if recall is ever represented as an erasure/privacy control, the asymmetry (metadata retained, bytes destroyed) is the worst of both worlds for both retention claims and evidence claims. GDPR angle only if GDPR applies (unknown).
- **Remediation** (security-engineer options): redact `File`/`Voice` blocks in the recall snapshot (keep text evidence, drop blob refs), or protect `message_edits`-referenced blobs (add `message_edits` to both the enqueue-time and drain-time reference scans). Option 2 changes the GC contract for deduped blobs (needs care); option 1 is simpler and matches minimization.
- **Evidence needed for closure**: regression test (recall a message with an attachment → assert either snapshot has no `blob_id` or `blob_gc_drain` leaves the blob while history references it), re-run storage recall 9/9 + blob GC tests + full gates.

### F2 — MEDIUM (open from r1, re-verified) · Deferred GDPR erasure net misses recalled messages

- **Verified evidence**: `participant.rs:644` — `sweep_deferred_erasure` predicate `(m.searchable_text <> '' OR m.embedding IS NOT NULL)`; recall clears both fields, so a message recalled **while under legal hold**, whose sender is later deleted, is skipped forever by the post-hold completion sweep while its pre-recall snapshot stays in `message_edits`. Zero references to `recalled_at`/`recalled_by` anywhere in `participant.rs`.
- **Risk**: indefinite retention of a deleted person's content (the exact scenario Art. 17(3)(e) defers, never completed). Narrow but real; non-compliance only if GDPR applies (unknown).
- **Remediation**: extend the deferred-sweep predicate with `recalled_at IS NOT NULL` and anonymize `message_edits` for that set (same pattern as the primary erasure).
- **Closure evidence**: migration + test (recall → hold → hold release → tombstoned sender → edits anonymized) + run.

### F3 — LOW (NEW) · `changes_since` 200-row clamp makes recall convergence bounded, not absolute

- **Verified evidence**: `query.rs:123-134` — `changes_since` clamps to 200 and returns one page; web client advances its cursor from the reply and never loops (per async review and db-architect F2). >200 mutations (incl. recalls) in a reconnect window are skipped; the offline member keeps rendering pre-recall content until reload/full history fetch.
- **Risk**: the "recalled content no longer displayed to members" claim has a bounded exception in high-churn rooms; not a data-protection breach (recall ≠ erasure; history retains originals anyway), but a controls-accuracy issue if recall convergence is asserted in any notice or audit narrative.
- **Remediation**: page the cursor (loop until caught up) or return a "truncated" flag forcing a full re-fetch; add a regression test with >200 mutations incl. a recall.
- **Closure evidence**: test + code change.

### F4 — LOW (open from r1) · Recall window undefined (policy gap, not code gap)

- Unbounded window maximizes privacy utility but gives no predictability for eDiscovery/legal-hold or for admin-compromise abuse (recall of arbitrarily old history). No recorded product/legal decision.
- **Closure evidence**: decision record (unbounded OK? configurable window? default?) + policy section.

### F5 — LOW (NEW, scale-conditional) · Migration 0238 runs under 10 s statement timeout

- **Verified evidence**: `db.rs:31` `SET statement_timeout='10000'` via `after_connect`; server boot runs `migrate(&pg)` on that pool (`boot/persistence.rs:33`). `CREATE INDEX idx_messages_room_mutated` on a production-sized `messages` can exceed 10 s → migration chain aborts. Impact is deploy availability (DDL is transactional in PG, so abort = clean rollback of that migration + fail-loud boot), not silent corruption — but the recall control cannot go live on large tables without a migrate-path timeout override or `CONCURRENTLY` (unusable under sqlx tx).
- **Closure evidence**: run 0238 against a representative-volume table (or a documented timeout override for the migrate path); benchmark note in the runbook.

### F6 — LOW (open from r1) · Recall ≠ erasure and the residual copy is member-readable, without disclosure

- Pre-recall text remains retrievable by every room member via the history route; audit digest retains 120 chars; webhook/processor and client caches are irrevocable. Combined with F1, the member-visible history now retains **text but not attachment bytes** — an inconsistent evidence story that needs a product decision either way.
- **Closure evidence**: privacy-notice diff + decision record (restrict history to admins/legal-hold, or disclose and keep); disclosure copy in UI.

### F7 — LOW (open from r1) · Abuse detection for recall has no alert

- `aero_messages_recalled_total` exists; no alert rule or runbook entry in tree; recall unthrottled on both entry points (WS lacks `check_ws_rate_room`, matching delete precedent; admin mass-recall uncapped).
- **Closure evidence**: alert rule on recall rate per actor/workspace + IR runbook entry (evidence sources: audit rows, outbox, NATS).

### F8 — LOW (open from r1) · Audit digest retains 120 chars of original content in plaintext

- Minimization compromise if audit access is broader than privileged-only or audit retention outlives message retention.
- **Closure evidence**: audit access-control test + retention policy; consider HMAC digest.

### Informational (record only, no action)

- **Existence oracle**: 404 vs 403 leaks cross-tenant existence (pre-existing on edit/delete; ULIDs unguessable). Plan §4.3 wording "防存在性 oracle" overstates — needs doc correction (security F2).
- **`recalled_by` outside erasure set**: consistent with the tombstone-stance for `sender_id` (participants are tombstoned, never hard-deleted; FK `NO ACTION` safe) but undocumented — add an explicit stance note (DB-architect F4).
- **Rolling-deploy poison ack-drop** of `kind:"recalled"` by old durable consumers — DS-4 release-note discipline already documented; deploy ordering is the control.
- **No browser E2E** for the recall UI (documented residual risk; covered by frame contract + render tests + web-check) — acceptable for this environment, note in the test strategy.
- **Webhook/processor irrevocability** and **client-side irrevocability** (already-rendered copies) belong in the processor register and user documentation respectively.

---

## 4. Audit-readiness summary

**Required documents before an audit can rely on recall controls:**
1. **Privacy notice / feature disclosure** (F6): recall is placeholder-only; originals remain in member-visible history (text; attachments currently destroyed — see F1), audit, and processor copies.
2. **Data-inventory entry for recall**: `messages.recalled_at/recalled_by`, `message_edits` snapshot, audit digest, outbox/NATS events, WS frames — with retention for each (F1, F2, F5).
3. **Retention policy**: recall-window decision (F4); audit-digest retention (F8); attachment-byte lifecycle decision — keep or destroy, and keep must be consistent with the snapshot (F1).
4. **Erasure proof**: test evidence that GDPR erasure covers recalled rows and their snapshots including the post-legal-hold edge (F2); explicit stance on `recalled_by` retention.
5. **IR runbook entry** for mass-recall abuse (F7) and for migration-timeout on large tables (F5).

**Next decisions:**
- (a) **Engineering, P0**: F1 — redact or protect recalled-attachment blobs; regression test first. (Security-engineer P0, concur.)
- (b) **Engineering**: F2 deferred-erasure predicate extension (small, self-contained); F3 `/changes` paging.
- (c) **Product/legal**: member visibility of pre-recall history — keep (disclose) or restrict (F6); recall window (F4).
- (d) **Declaration**: jurisdiction/framework applicability — the single highest-leverage unknown; nothing in the repo asserts which regime applies, so GDPR-informed controls remain design assumptions, not compliance.

**Validation methods used**: source inspection of every load-bearing path (recall tx, GC checks, history route, erasure sweeps, `changes_since`, migration, boot wiring), cross-check of round-2 security/database reviews, gate evidence from the implementer artifact (not re-run here). **Not performed**: live DB replay, browser E2E, penetration testing — none available in this environment. This review is an engineering assessment, not legal advice; repository controls alone are not evidence of certification, and no framework applicability is asserted here.
