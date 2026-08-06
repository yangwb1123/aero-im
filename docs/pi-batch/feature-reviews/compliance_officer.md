`prompts/README.md` does not exist in this repo (checked `/home/u1/aero-im/prompts/` — no such directory), so the priming instruction could not be applied; the review below stands on the evidence supplied plus direct source inspection. Verified against: `migrations/0238_message_recall.sql`, `aero-im-core/src/service/messages.rs` (`recall_message`), `aero-storage/src/message/authorization.rs` (`recall_outboxed_authorized`, `recall_locked_outboxed_in_tx`, `recall_role_allowed_in_tx`), `recall_tests.rs`, `message_history.rs`, `message_edit.rs`, `participant.rs` (erasure), `conversation_export.rs`, `webhooks.rs`, `bot_dispatch.rs`, `web/render.js`, `web/app.js`, `ws/frame.rs`.

---

# Compliance Review — Message Recall (消息撤回)

## 1. Applicable-scope statement

**Scope.** The recall subsystem: REST `POST /api/messages/:id/recall` + WS `recall_message`, transactional recall (migration 0238), `Recalled` room event broadcast, client placeholder rendering, and the data lifecycle it touches (messages, `message_edits`, audit, embeddings/search, attachments, outbox/NATS, webhooks).

**What the evidence does NOT establish (marked unknown, per instructions):**
- **Jurisdiction**: unknown. No deployment jurisdiction declared anywhere in the evidence.
- **Data classification**: unknown. No classification of recalled content (e.g., employee comms vs. customer PII) is asserted.
- **Framework applicability**: unknown. Code comments reference GDPR concepts (Art. 17(3)(e) legal-hold exemption in `participant.rs` erasure paths), which is *evidence of GDPR-informed design*, not a legal determination that GDPR applies to this deployment. SOC 2 / ISO 27001 / HIPAA / other frameworks: not referenced — excluded from scoring, listed as "not established."
- **Excluded**: no new vendors are introduced by recall; no new cryptographic material; encryption/key-management obligations are unchanged from the base system and are noted as "carried, not re-assessed."

**Key characterization (load-bearing for everything below):** recall is a **display-level placeholder operation, not erasure**. Original content survives in `message_edits` (viewable by any room member via `GET /api/messages/:id/history`), in the `message.recalled` audit digest (120 chars), and irrevocably in webhook-consumer and client caches. This is by design ("row/history/audit intact") and Slack-like, but it means **recall cannot satisfy an erasure request and the UI wording "撤回" overstates the effect**.

---

## 2. Control matrix

| # | Requirement | Status | Repository evidence | Process evidence | Gap | Owner | Validation |
|---|---|---|---|---|---|---|---|
| 2.1 | Author-only or owner/admin recall (least privilege) | **Implemented** | `recall_authorized` pure fn + table-driven matrix test; commit-time recheck `recall_role_allowed_in_tx` under `FOR UPDATE` on `room_members` | `recall_permission_matrix` (5×actor×role), `recall_permission_matrix_author_admin_owner_member` | None found | Backend eng | `cargo test --workspace --lib`; re-run matrix test |
| 2.2 | Tenant isolation / no cross-tenant oracle | **Implemented** | `assert_room_access` before state checks; 404→403→409→409→403 stable ordering; doc: "no existence oracle" | `recall_cannot_cross_workspace_boundaries` | None found | Backend eng | storage db_tests (ignored, `DATABASE_URL`) |
| 2.3 | Race/TOCTOU resistance | **Implemented** | Row locks: message `FOR UPDATE`, membership `FOR UPDATE`, identity re-validation (`room/sender` unchanged), `WHERE recalled_at IS NULL AND deleted_at IS NULL` final fence | `recall_outboxed_authorized` returns `None` on raced mutation → `Conflict` | None found | Backend eng | db_tests race cases |
| 2.4 | REST/WS single invariant | **Implemented** | Both paths call `ImService::recall_message`; frame contract test `recalled_frame_shape_carries_placeholder_message` | WS/REST convergence noted in handler doc | None found | Backend eng | ws frame test |
| 2.5 | Audit trail | **Implemented** | `message.recalled` audit row (workspace, actor, message id, 120-char content digest), same tx as mutation | `append_in_tx` in recall tx; `message.deleted` precedent | **Digest retains 120 chars of original content in audit**; audit-retention policy not evidenced | Platform/legal | Audit query + retention policy review |
| 2.6 | Data minimization on live surfaces | **Implemented** | Placeholder `blocks`, `searchable_text=''`, `embedding=NULL`, `enqueue_unreferenced_blobs` (dedup-aware GC) | `recall_locked_outboxed_in_tx` | Embedding backfill correctly excludes recalled (`searchable_text <> ''` predicate) — verified, no gap | Backend eng | Code inspection |
| 2.7 | Retention/deletion lifecycle | **Implemented w/ edge gap** | Recalled rows remain subject to retention sweep/ephemeral expiry and moderation delete (`recalled_message_can_still_be_deleted`); GDPR erasure anonymizes recalled rows + their `message_edits` snapshots (keyed on erased message set) | `participant.rs` erasure tx | **Deferred post-legal-hold erasure sweep predicates on `searchable_text <> '' OR embedding IS NOT NULL` — recalled messages (both empty) are skipped, leaving the pre-recall snapshot in `message_edits` indefinitely after hold release** | Platform eng | See Finding 2 |
| 2.8 | Content availability boundary documented | **Gap** | `message_history.rs` route is member-gated; `list_for_message` returns pre-recall snapshot for recalled (non-deleted) messages | — | **Any room member can retrieve original content of a recalled message via history API; no UI disclosure, no privacy-notice language** | Product + legal | See Finding 1 |
| 2.9 | Recall window / policy | **Gap (open decision)** | Doc: "no time limit on recall window — product decision left open" | `docs/pi-batch/feature-plan.md` | No policy, no admin-configurable window, no documented rationale | Product owner | See Finding 3 |
| 2.10 | Irrevocability & user notice | **Implemented** | Recall terminal (edit/react/reply/recall disabled in `render.js`), no un-recall; placeholder "此消息已被撤回" | render.js + web-check | No user-facing notice that history retains originals | Product | See 2.8 |
| 2.11 | Detection/observability | **Implemented** | `aero_messages_recalled_total` + `MESSAGE_PROCESSING_DURATION_SECONDS{op=recall}` | metrics.rs | No alert/runbook for anomalous bulk-recall (abuse of admin recall capability) | SRE | Prometheus alert rule |
| 2.12 | Third-party data flow | **Partially documented** | `webhooks.rs` maps `Recalled` → `"recalled"` type (placeholder body delivered at recall time) | bot_dispatch maps `Recalled`; bots act only on `Message` (no re-processing) | **Processor inventory must record that webhook consumers received the original at send time and cannot be un-sent** | Privacy/legal | Processor register |
| 2.13 | Encryption / key mgmt | **N/A (carried)** | No new keys, no new storage tier | — | Re-assess only at deployment level | Infosec | Existing crypto review |
| 2.14 | Client-side irrevocability | **Inherent limitation** | — | — | Recalled content already rendered on other clients cannot be revoked; must be in user documentation | Product | Doc review |

---

## 3. Findings

**Finding 1 — Medium · Recall ≠ erasure, and the residual copy is member-readable**
- *Risk*: A user recalling a message reasonably believes content is withdrawn. Original content remains retrievable by **every room member** through `GET /api/messages/:id/history` (`message_history.rs` + `list_for_message`, live because `deleted_at IS NULL`), and in the audit digest. Regulatory exposure: privacy-notice accuracy, misleading-design concerns, and inability to honor "withdraw my content" expectations; if the deployment processes personal data, a regulator could treat recall as a partial erasure control that doesn't deliver what it promises.
- *Remediation*: (a) Product decision: either restrict history access for recalled messages (e.g., admins/legal-hold only) or keep member-visible history and disclose it; (b) add disclosure copy in UI and privacy notice ("撤回后，消息内容仍可能被房间成员通过历史记录查看"); (c) document recall as display-control, not erasure, in the feature spec.
- *Evidence needed for closure*: decision record + privacy-notice diff + (if restricted) authz change + test.

**Finding 2 — Medium/Low · GDPR deferred-erasure net misses recalled messages**
- *Risk*: The post-legal-hold completion sweep (`participant.rs`, predicate `m.searchable_text <> '' OR m.embedding IS NOT NULL`) excludes recalled messages (both fields cleared at recall). A message recalled **while under legal hold**, whose sender is later deleted, keeps its pre-recall snapshot in `message_edits` after hold release — indefinite retention of a deleted person's content. Narrow but real; Art. 17 non-compliance if GDPR applies.
- *Remediation*: Extend the deferred sweep predicate to include `recalled_at IS NOT NULL`, and anonymize `message_edits` for that set (same pattern as the primary erasure).
- *Evidence needed for closure*: migration + test (`recalled + hold release + tombstoned sender → edits anonymized`) + run.

**Finding 3 — Medium · Undefined recall window is a policy gap, not a code gap**
- *Risk*: Unbounded window maximizes privacy utility but gives no predictability for eDiscovery/legal-hold or for abuse scenarios (an admin compromise can recall arbitrary history at any age). Conversely a bounded window would trade away privacy utility. The absence of a recorded decision is the finding.
- *Remediation*: Record a product/legal decision (unbounded OK for your jurisdiction? admin-configurable window? default?), document in policy, and surface the setting if configurable.
- *Evidence needed for closure*: decision record; policy section.

**Finding 4 — Low · Audit digest retains 120 chars of original content**
- *Risk*: `message.recalled` audit rows embed a content prefix; if audit access is broader than needed or audit retention outlives message retention, minimization is compromised.
- *Remediation*: Document audit retention/access policy; consider hashing the digest (HMAC) instead of plaintext prefix; confirm audit reads are privileged-only.
- *Evidence needed for closure*: audit access-control test + retention policy.

**Finding 5 — Low · Abuse detection for recall lacks an alert**
- *Risk*: Recall is terminal for user content; a compromised admin can mass-recall. Metrics exist but no alert/runbook.
- *Remediation*: Alert on `aero_messages_recalled_total` rate per actor/workspace; add recall to incident-response runbook (evidence: audit rows, outbox, NATS).
- *Evidence needed for closure*: alert rule + runbook entry.

**Informational (no action, record only):**
- No browser E2E for recall UI (documented residual risk; render path + frame contract + web-check cover statically).
- `recalled_by` FK has no `ON DELETE`; safe today because participants are tombstoned, not hard-deleted — flag if a hard-delete path is ever added.
- Webhook/processor copies of pre-recall content are inherently irrevocable — belongs in the processor register (Finding 2.12).

---

## 4. Audit-readiness summary

**Required documents before an audit can rely on recall controls:**
1. Privacy notice / feature disclosure: recall is placeholder-only; originals remain in member-visible history, audit, and processor copies (Finding 1, 2.12).
2. Data inventory entry for recall: `messages.recalled_at/recalled_by`, `message_edits` snapshot, audit digest, NATS/outbox events, WS frames — with retention for each.
3. Retention policy: recall window decision (Finding 3); audit-digest retention (Finding 4).
4. Erasure proof: test evidence that GDPR erasure covers recalled rows and their snapshots, including the post-hold edge (Finding 2).
5. Incident-response runbook entry for mass-recall abuse (Finding 5).

**Next decisions:**
- (a) Product/legal: member visibility of pre-recall history — keep or restrict (blocks Finding 1 closure).
- (b) Product/legal: recall window — unbounded, bounded, or configurable (blocks Finding 3).
- (c) Engineering: fix deferred-erasure predicate for recalled rows (small, self-contained).
- (d) Jurisdiction/framework declaration — the single highest-leverage unknown: nothing in the repo asserts which regime applies; without it, GDPR-informed controls are design assumptions, not compliance.

**Validation methods used here:** source inspection (migration, service, storage tx, routes, client), test inventory (`recall_tests.rs` ×5, permission matrix, frame contract), and cross-checks against erasure/retention/export paths. **Not performed:** live DB replay, browser E2E, penetration testing — none exist in this environment. This review is an engineering assessment; it is not legal advice, and repository controls alone are not evidence of certification.
