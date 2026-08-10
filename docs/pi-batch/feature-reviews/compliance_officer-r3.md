# Compliance Review (Round 3) — Message Recall (消息撤回), gate B1/B2 round

- **Date**: 2026-08-06 (round 3; supersedes `compliance_officer-r2.md`, which is preserved)
- **Priming**: `prompts/README.md` does not exist in this repo (verified `/home/u1/aero-im/prompts/` absent). Applied the shared rules at `/home/u1/ai-batch-runner/prompts/README.md` (present, read in full). Implemented controls are not evidence of certification.
- **Method**: re-derived the round-4 delta and every load-bearing carried claim from current source, not from prior reports: `crates/aero-storage/src/message/mod.rs:187` (`redact_blocks_for_recall_snapshot`), `message/authorization.rs:282-335` (recall tx: redacted snapshot → placeholder UPDATE → GC enqueue of **original** blob ids → audit → outbox), `message/orig.rs:69` (hermetic B1 unit test), `message/recall_tests.rs` (PG regression), `web/package.json:8` (B2), `migrations/0238_message_recall.sql` (re-read; still **untracked**), `participant.rs:575/644/660` (erasure + deferred sweep), `message_history.rs:64` (history gate), `ws/ws_impl/frame.rs:157` (WS recall dispatch — **no** `check_ws_rate_room`), `query.rs` (`changes_since`). Cross-checked the round-4 security engineer and database architect reviews; their load-bearing claims re-verified independently where compliance-relevant (B1 containment, DB-F1 tie-break, DB-F2 `message_edits` lifecycle, DB-F3 migration timeout).
- **This round's gate evidence** (implementer artifact, not re-run by this reviewer): check/clippy `-D warnings` green, lib tests 2159/0, web 82/0, authz_lint 6/6, fresh-DB 238-migration replay + full ignored suite EXIT 0, backend-quality 0, storage recall 10/10, B1 unit test red-before-green, `COMPLETION: OK`. Feature remains **uncommitted** (verified `git status`: `?? migrations/0238_message_recall.sql` + modified files).

---

## 1. Applicable-scope statement

**In scope.** The recall subsystem and the data lifecycle it touches: `messages` (incl. new `recalled_at`/`recalled_by`), `message_edits` snapshots (now redacted), `audit_log` digest, `event_outbox`/NATS `Recalled` events, WS frames, blob GC (`blob_gc_queue`), search/embedding clearing, `changes_since` replay, GDPR erasure paths (`participant.rs`), webhook/bot consumers, and the member-visible history route.

**Unknown (marked per instructions — the evidence does not establish them):**
- **Jurisdiction**: unknown. No deployment jurisdiction declared anywhere in the evidence.
- **Data classification**: unknown. No classification of recalled content (employee comms, customer PII, minors, etc.) is asserted.
- **Framework applicability**: unknown. `participant.rs` cites GDPR Art. 17(3)(e) in a code comment (legal-hold exemption) — evidence of GDPR-informed design, not a legal determination that GDPR applies. SOC 2, ISO 27001, HIPAA, LGPD, CCPA/CPRA, eDiscovery regimes: not referenced anywhere — excluded from scoring, listed as not established.
- **Lawful purpose / purpose limitation**: unknown. No privacy-impact analysis, purpose register, or lawful-basis record exists in-repo for any feature; recall is reviewed purely as a data-lifecycle control.

**Excluded (carried from r2, unchanged by this round):** no new vendors, no new cryptographic material, no new storage tier, no new keys. Encryption/key-management obligations are inherited; deployment-level gaps are flagged in §3.2, not scored against this feature.

**Load-bearing characterization (carried, now internally consistent):** recall is a **display-level placeholder operation, not erasure**. Pre-recall **text and voice-transcript evidence** survives in `message_edits` (member-visible via `GET /api/messages/:id/history`, `message_history.rs:64` gates with `assert_room_access` only), in the audit digest (first 120 chars), and irrevocably in webhook-consumer and client caches. Round-4 B1 resolves the r2 contradiction for **attachment bytes**: the snapshot now carries **no** `blob_id` (bytes are destroyed by GC, text/transcript evidence is retained by design). The evidence story is now consistent — but recall still cannot satisfy an erasure request, and **voice transcripts are content-derived data that survive recall**, which must be disclosed if recall is ever represented as a privacy control.

---

## 2. Control matrix

Status: ✅ implemented & verified · ⚠️ implemented with gap · ❌ gap/open decision · ➖ carried/inherited. **CLOSED** marks round-4 resolutions.

| # | Requirement | Status | Repository evidence | Process evidence | Gap | Owner | Validation |
|---|---|---|---|---|---|---|---|
| 2.1 | Least privilege: author or owner/admin only | ✅ | `recall_authorized` pure fn (im-core `messages.rs`); commit-time role re-check under `FOR UPDATE` (`authorization.rs`) | Table-driven permission matrix tests | None | Backend eng | Re-run storage/im-core ignored suites |
| 2.2 | Tenant isolation: `assert_room_access` before any state check | ✅ | Service order get→guard→deleted→recalled→role; in-tx `aero_effective_room_access` re-check | Cross-workspace + cross-room tests | Plan wording "防存在性 oracle" overstates: 404 vs 403 leaks existence (pre-existing, ULIDs unguessable) | Backend eng | Doc fix (carried) |
| 2.3 | TOCTOU/race resistance | ✅ | Row lock + membership lock + identity re-validation + final `WHERE recalled_at IS NULL AND deleted_at IS NULL` fence; `version+1` | `concurrent_double_recall_has_exactly_one_winner`; one outbox row | None | Backend eng | Ignored db_tests |
| 2.4 | REST + WS converge on one invariant | ✅ | Both paths → `ImService::recall_message`; frame contract test; 409→success client mapping | ws/api/render tests (82/0 web, incl. B2) | None | Backend eng | `node --test web/` + `npm test` |
| 2.5 | Audit trail, same tx | ✅ | `audit_log` `message.recalled` (workspace, actor, id, **120-char plaintext digest**) in recall tx (`authorization.rs`) | `author_recall_replaces_content_and_records_audit_in_one_tx` | Audit access/retention policy not evidenced | Platform/legal | Finding 3.1-8 |
| 2.6 | Data minimization on live surfaces | ✅ | Placeholder `blocks`, `searchable_text=''`, `embedding=NULL`; backfill/embedding jobs exclude | Recall tx test asserts placeholder + cleared fields | None | Backend eng | Code inspection |
| 2.7 | Attachment byte disposal (minimization half) | ✅ **CLOSED** | `enqueue_unreferenced_blobs_in_tx` enqueues the **original** `blob_id`s (`authorization.rs:330`); drain delete-then-ack with `has_live_references` | `recall_snapshot_redacts_blob_references_and_gc_proceeds`: both blobs queued; B1 unit test red-before-green | None | Backend eng | Storage recall 10/10 (Verified) |
| 2.8 | Evidence/retention integrity of the snapshot | ✅ **CLOSED** | `redact_blocks_for_recall_snapshot` (`mod.rs:187`): File→`[附件已移除]`, Voice+transcript→text, Voice w/o→`[语音已移除]`, others unchanged; **output never contains `blob_id`**; snapshot insert uses it (`authorization.rs:288`) | Regression asserts zero `"blob_id"` occurrences in snapshot JSON + transcript preserved | **Edit-path snapshots still retain `blob_id`** (edit never GCs replaced blobs — pre-existing leak, documented, out of scope) | Backend eng | Storage recall 10/10 (Verified); SQL check 5 (query #5 in DB-architect §4) |
| 2.9 | Retention/deletion lifecycle incl. GDPR erasure | ⚠️ | Primary erasure anonymizes recalled rows + their `message_edits` (`participant.rs:575,660`) | `recalled_message_can_still_be_deleted` | **Deferred-erasure sweep skips recalled rows** (`participant.rs:644` predicate `searchable_text <> '' OR embedding IS NOT NULL`; recall clears both) → post-legal-hold completion never fires for recalled content | Platform eng | Finding 3.1-2 |
| 2.10 | Offline/reconnect convergence of recall | ⚠️ | `changes_since` uses `GREATEST(edited_at, deleted_at, recalled_at)` + reissued index; cursor hygiene fix in `ws.js` | `changes_since_delivers_recalls` green | **No keyset tie-break**: set-based sweeps stamp identical `NOW()` on >200 rows → page-2 `> since` excludes all remaining rows; "≤200 mutations guaranteed convergence" claim is false under bulk same-timestamp sweeps | Backend eng | Finding 3.1-3 (upgraded from r2-F3) |
| 2.11 | Recall window / policy | ❌ | Plan: "no time limit on recall window — product decision left open" | — | No policy, no configurable window, no recorded rationale | Product owner | Finding 3.1-4 (open from r1/r2) |
| 2.12 | User notice / disclosure | ❌ | Placeholder `[此消息已被撤回]`; recall terminal (edit/react/reply suppressed) | — | No user-facing notice that pre-recall **text and transcripts** remain in member-visible history, audit, and processor copies; UI wording "撤回" overstates | Product + legal | Finding 3.1-6 (open; content of residual copy changed by B1) |
| 2.13 | Abuse detection / observability | ⚠️ | `aero_messages_recalled_total` + histogram op `recall` | — | **Recall unthrottled on both entry points** — WS `ClientFrame::RecallMessage` (`frame.rs:157`) has no `check_ws_rate_room` (verified), REST uncapped; no alert/runbook on recall rate per actor/workspace | SRE | Finding 3.1-7 (open from r1) |
| 2.14 | Third-party (webhook/processor) data flow | ⚠️ | `webhooks.rs` maps `Recalled` → `"recalled"` (placeholder body at recall time); bots act only on `Message` | — | Processor register must record consumers received the original at send time; client-rendered copies irrevocable | Privacy/legal | Processor register (open from r1) |
| 2.15 | Migration/rollout reliability of the control | ⚠️ | 0238 additive (2 nullable cols + CHECK extension + shadow reconcile + index/backfill reissue); DS-4 upgrade-all-nodes-first | Fresh-DB 238-migration replay green (empty DB) | **Migration pool runs `statement_timeout=10s`** (`db.rs`) → 0238 `CREATE INDEX` can abort on production-sized `messages`; **feature + 0238 uncommitted → CI has never run on recall code**; version-skew ack-drop enforced by discipline only | Platform eng | Finding 3.1-5 + Finding 3.2-2 |
| 2.16 | Encryption / key management | ➖ | No new keys/tiers this round | — | Deployment-level: dev-only JWT keys, no rotation, no KMS/Vault definitions (devops F5) | Infosec | Re-assess at deployment level |
| 2.17 | Blob-GC containment vs evidence consistency (B1) | ✅ **CLOSED** | Snapshot redacted ⇒ `message_edits` invisible to GC scan is now correct by construction; drain-time `has_live_references` scans `messages`/emoji/ledger only — no dangling history refs possible | Regression: snapshot has no `blob_id` + transcript kept + GC queue has both blobs + live row == placeholder | None | Backend eng | Storage recall 10/10 + SQL check #5 |
| 2.18 | Test-in-CI coverage of recall rendering (B2) | ✅ **CLOSED** | `web/package.json:8` test script includes `render_recall.test.js` | `cd web && npm test` 82/0 (was not running the file before) | CI still never runs it: feature uncommitted (Finding 3.2-2) | Feature owner | `npm test` after commit |
| 2.19 | `message_edits` lifecycle (retention/erasure completeness) | ⚠️ **NEW** | `0036_message_edits.sql`: `message_id` **no FK**; ephemeral sweep hard-deletes `messages` with no `message_edits` cleanup; cutover migrations 0148/0174/0238 mirror live columns only | — | Recalled/edited ephemeral messages leave orphan evidence rows forever; cutover runbook has no defined fate for `message_edits`/`message_reports` | Platform eng | Finding 3.1-1 |

---

## 3. Findings

### 3.1 Feature-level

**F1 — MEDIUM · `message_edits` has no lifecycle: no FK, no sweep, no cutover story (NEW, DB-architect F2, re-verified relevant)**
- **Verified evidence**: `0036_message_edits.sql` declares `message_id uuid NOT NULL` with no `REFERENCES messages`; ephemeral sweep (`sweep.rs` `DELETE FROM messages WHERE id = ANY($1)`) has no `message_edits` cleanup; migrations 0148/0174/0238 mirror live `messages` columns but never mention `message_edits`/`message_reports`. Recall adds one evidence row per recalled message, so the feature grows the orphan population.
- **Business/regulatory risk**: unbounded retention of evidence rows for content that was intentionally ephemeral or deleted; at the documented partition cutover, history rows are either silently orphaned or dropped depending on an unstated runbook choice — either way the retention story (what survives, for how long, under which legal regime) is undefined. GDPR angle only if GDPR applies (unknown).
- **Remediation**: `REFERENCES messages(id) ON DELETE CASCADE` (safe now that recall snapshots are redacted — B1 makes cascade deletion of redacted text correct) or an explicit sweep; add `message_edits` + `message_reports` to the cutover runbook checklist.
- **Closure evidence**: migration + `SELECT count(*) FROM message_edits e LEFT JOIN messages m ON m.id=e.message_id WHERE m.id IS NULL;` regression asserting 0 after ephemeral delete + cutover checklist update.

**F2 — MEDIUM · Deferred GDPR erasure net misses recalled messages (open from r1/r2, re-verified)**
- **Verified evidence**: `participant.rs:644` — `sweep_deferred_erasure` predicate `(m.searchable_text <> '' OR m.embedding IS NOT NULL)`; recall clears both → a message recalled **while under legal hold**, whose sender is later deleted, is skipped forever by the post-hold completion sweep while its pre-recall snapshot stays in `message_edits`. Zero references to `recalled_at`/`recalled_by` in `participant.rs`.
- **Risk**: indefinite retention of a deleted person's content (the exact scenario Art. 17(3)(e) defers, never completed). Narrow but real; non-compliance only if GDPR applies (unknown). Note the B1 change slightly reduces scope: attachment bytes are already gone; the indefinite residue is now text/transcript evidence only.
- **Remediation**: extend the deferred-sweep predicate with `recalled_at IS NOT NULL` and anonymize `message_edits` for that set (same pattern as primary erasure).
- **Closure evidence**: migration + test (recall → hold → hold release → tombstoned sender → edits anonymized) + run.

**F3 — MEDIUM · `changes_since` has no tie-break; bulk same-timestamp sweeps skip mutations on reconnect (upgraded from r2-F3, Low → Medium; DB-architect F1, re-verified)**
- **Verified evidence**: `query.rs` — `WHERE ... GREATEST(edited_at, deleted_at, recalled_at) > $2 ORDER BY GREATEST(...) ASC LIMIT 200` (clamp 1..200); client fetches one page, advances cursor, never loops. Set-based sweeps stamp `deleted_at = NOW()` per statement (`crud.rs:263`, `events.rs:320`): a sweep of >200 messages in one room yields >200 rows with an **identical** `GREATEST`; page 2's `> since` excludes all of them. The round-3 gate's claim "≤200 变更/重连窗口内收敛有保证" is therefore inaccurate even under 200 mutations.
- **Risk**: offline/reconnect members can keep rendering pre-recall content indefinitely in high-churn rooms — exactly the class of "recalled content still displayed" defect the feature was built to close. Not data loss; not a data-protection breach on its own (recall ≠ erasure), but it invalidates any convergence assertion in notice/audit narrative. Affects deleted/edited convergence equally; recall is the sensitive case.
- **Remediation**: keyset tie-break `(GREATEST(...), id) > ($2, $3)` + composite index `(room_id, GREATEST(...), id)`; or stagger sweep timestamps per row; or client paging loop.
- **Closure evidence**: PG regression with 250 rows sharing one `deleted_at`; assert all converge across pages, incl. a recall in the batch.

**F4 — LOW · Recall window undefined (open from r1)**
- Unbounded window maximizes privacy utility but gives no predictability for eDiscovery/legal-hold or admin-compromise abuse. No recorded product/legal decision. **Closure**: decision record + policy section.

**F5 — LOW (scale-conditional) · Migration 0238 runs under 10 s statement timeout (open from r2; DB-architect F3 re-confirmed as Medium-operational)**
- `db.rs` `SET statement_timeout='10000'` via `after_connect` on the migration pool; 0238's `CREATE INDEX idx_messages_room_mutated` can exceed 10 s on production-sized `messages` → chain aborts mid-deploy (transactional DDL = clean rollback + fail-loud, not corruption). **Closure**: `SET LOCAL statement_timeout = 0` for heavy migrations or a migrate-path knob; benchmark at representative volume.

**F6 — LOW · Recall ≠ erasure, residual copy member-readable, no disclosure (open from r1; content changed by B1)**
- Post-B1 residual copy is now **text/transcript-only, no attachment metadata**: pre-recall text and voice transcripts remain retrievable by every room member via the history route; audit digest retains 120 chars; webhook/client copies irrevocable. Internally consistent, but the product must decide and disclose: history stays member-visible (disclose) or restricted to admins/legal-hold. **Note for disclosure copy**: voice transcripts are content derived from audio — their survival must be called out, not just text. **Closure**: privacy-notice diff + decision record + UI disclosure copy.

**F7 — LOW · Recall unthrottled, no abuse alert (open from r1; re-verified)**
- Verified: WS `RecallMessage` dispatch (`frame.rs:157`) has no `check_ws_rate_room` (edit does); REST uncapped; `aero_messages_recalled_total` exists but no alert/runbook. Matches delete precedent; admin mass-recall uncapped and unalerted. **Closure**: alert rule on recall rate per actor/workspace + IR runbook entry.

**F8 — LOW · Audit digest retains 120 chars of original text in plaintext (open from r1)**
- Minimization compromise if audit access is broader than privileged-only or audit retention outlives message retention. **Closure**: audit access-control test + retention policy; consider HMAC digest.

### 3.2 Deployment-level (carried; compliance-relevant, out of feature scope)

**D1 — HIGH · No backup/restore for stateful stores (devops F4).** No pg_dump/WAL automation, no Redis AOF policy in prod, no NATS JetStream backup (durable consumer cursor + stream are replay/ordering fact sources). Business/regulatory risk: no continuity path for the very data recall/erasure controls operate on; a restore drill is impossible to evidence. **Closure**: nightly PG dump + JetStream snapshot, documented RPO/RTO, quarterly restore drill.

**D2 — HIGH · Feature uncommitted; CI has never evaluated recall (devops F2).** Verified: `?? migrations/0238_message_recall.sql` + ~30 modified files; no git tags. The 10/10 storage suite, 238-migration replay, and `npm test` evidence are local-only. Any audit or release claim about recall controls must rest on `main` CI evidence. **Closure**: commit feature + migration, `main` CI green (integration job incl. recall ignored suite) as the promotion gate.

**D3 — MEDIUM · Version-skew ack-drop is documented, not enforced (devops F6; DB-architect F5).** Old binaries ack-drop `kind:"recalled"` and the durable cursor advances → permanent event loss for new readers during rolling deploys; mitigations are deploy-ordering discipline only. **Closure**: enforced minimum-version gate before cutover, or code-level ignore-but-ack mapping; mixed-fleet regression.

**D4 — MEDIUM · Secrets delivery is a fragment; JWT keys dev-only, no rotation (devops F5).** RS256 private key compromise = full session forgery; production secret delivery hand-jobs `AERO__AUTH__JWT_PRIVATE_KEY_PEM` etc. **Closure**: Vault/KMS path definitions + rotation runbook; secret-free deploy rehearsal.

**D5 — LOW · Monitoring/alerting are configs, not wiring (devops F7).** SLO alert rules have no owner/routing; recall-rate alert (F7 above) lands here. **Closure**: wire scrape + routing, on-call ownership, alert-fires-on-injected-5xx drill.

**Informational (record only, no action):**
- Existence oracle 404 vs 403 (pre-existing; ULIDs unguessable) — doc correction pending (security F2).
- `recalled_by` outside the erasure set — consistent with the `sender_id` tombstone stance but undocumented; add explicit stance note.
- Redis `seq` counter reset under RDB-only restore would violate per-subject monotonicity (DB-architect F4) — recovery check to document in the runbook.
- Rolling-deploy poison ack-drop and no-browser-E2E — documented residual risks; acceptable with deploy discipline and the frame-contract/render test coverage.
- Edit-path snapshots retain `blob_id` (edit never GCs replaced blobs) — pre-existing leak, documented, out of scope; if `message_edits` gains `ON DELETE CASCADE` (F1) the redaction invariant is what makes cascade deletion of recalled snapshots safe.

---

## 4. Audit-readiness summary

**Required documents before an audit can rely on recall controls:**
1. **Privacy notice / feature disclosure** (F6, updated for B1): recall is placeholder-only; pre-recall **text and voice transcripts** remain in member-visible history, audit digest (120 chars), and processor/client copies; attachment bytes are destroyed with no surviving metadata. UI copy must not overstate "撤回".
2. **Data-inventory entry for recall** (F1): `messages.recalled_at/recalled_by`, redacted `message_edits` snapshot, audit digest, outbox/NATS `Recalled` events, WS frames — with retention for each, incl. the `message_edits` lifecycle decision (cascade vs sweep) and the ephemeral-sweep orphan behavior.
3. **Retention policy**: recall-window decision (F4); audit-digest retention and access (F8); `message_edits`/`message_reports` fate at partition cutover (F1).
4. **Erasure proof**: test evidence that GDPR erasure covers recalled rows and snapshots incl. the post-legal-hold edge (F2); explicit stance on `recalled_by`.
5. **IR runbook entries**: mass-recall alert (F7), migration-timeout-at-scale (F5), backup/restore drill (D1), version-skew deploy gate (D3).
6. **Release evidence**: feature committed + CI green (D2), artifact provenance (devops F3), secrets plan (D4).

**Next decisions:**
- (a) **Engineering, P0 — none remaining in feature scope**: B1 and B2 are closed and correct by construction; r2's blocking item (snapshot↔GC dangling reference) is resolved. Confirm the three open engineering items (F3 tie-break, F2 deferred-erasure predicate, F1 `message_edits` lifecycle) are scheduled; none blocks this feature's controls.
- (b) **Engineering**: F3 (keyset tie-break + regression) — the highest-value correctness item since it undermines the feature's convergence claim; F2 (small, self-contained); F1 (FK + sweep decision).
- (c) **Product/legal**: member visibility of pre-recall text/transcripts — keep-and-disclose vs restrict (F6); recall window (F4); admin-recall surface confirmation.
- (d) **Declaration**: jurisdiction/framework applicability remains the single highest-leverage unknown; nothing in the repo asserts which regime applies, so GDPR-informed controls stay design assumptions, not compliance.

**Validation methods used**: source inspection of every load-bearing path (redaction fn + its call site, GC enqueue, placeholder UPDATE, audit, history gate, erasure sweeps, `changes_since`, WS dispatch, test wiring, migration), cross-check of round-4 security/database reviews, gate evidence from the implementer artifact (not re-run here). **Not performed**: live DB replay, browser E2E, penetration testing — none available in this environment. This review is an engineering assessment, not legal advice; repository controls alone are not evidence of compliance or certification, and no framework applicability is asserted here.
