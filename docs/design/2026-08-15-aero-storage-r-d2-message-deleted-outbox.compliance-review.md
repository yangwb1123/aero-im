# Compliance Review — R-D2: `message.deleted` → Governance Outbox

**Reviewer role:** compliance officer · **Revision:** design commit `e48a3c9` (change **not landed** — `soft_delete_locked_outboxed_in_tx` still discards the `AuditId`; no writer exists)
**Inputs:** `docs/design/2026-08-15-aero-storage-r-d2-message-deleted-outbox.design.md`; QA lead report (measured at HEAD); database-architect adversarial review; platform contract.
**Checks I personally ran (this revision):** read of `migrations/0239`, `migrations/0146`, `crates/aero-storage/src/message/authorization.rs:440-550`, `crates/aero-storage/src/message/events.rs:290-365`, `crates/aero-storage/src/audit_governance/outbox.rs:240-360,568-623`, `crates/aero-server/src/bin/boot/retention.rs`, `crates/aero-server/src/bin/boot/metrics_tasks.rs`, `crates/aero-storage/src/participant.rs:418-430`, `crates/aero-audit-connector/src/config.rs`, plus greps (`audit_governance_outbox` refs in boot/, legal-hold refs in message/authorization.rs, audit routes, `AERO_AUDIT_TOKEN_ENDPOINT`, classification consts). Test/drill results cited from the QA report are **measured by QA at this revision**, not re-run by me.
**Evidence labels:** Verified / Partial / Missing / Proposed / Unknown (per `prompts/README.md`). Verified = I or QA observed it in the tree at this revision; Proposed = designed but not landed.

> ⚠️ This review is advisory analysis from repository evidence. It is **not** legal advice, a compliance determination, or certification evidence. Implemented controls are not evidence of certification.

---

## 1. Applicable-scope statement, excluded and unknown frameworks

### 1.1 Scope of this review

The subsystem under review is the **governance audit pipeline for message deletion**: the pre-existing `message.deleted` audit append (which already runs in the delete transaction today) plus the **proposed** replication of that event into `audit_governance_outbox` by a Rust writer, and its relay to the external governance sink. The review covers the personal-data lifecycle of that flow (collection, minimization, retention, deletion, transfer), access control on the producing path, audit-evidence integrity, and monitoring.

### 1.2 Frameworks shown to apply (from the supplied inputs only)

| Framework / instrument | Basis for applicability | Status |
|---|---|---|
| **GDPR (EU) — Art. 5(1)(c),(e) minimization & storage limitation; Art. 17 erasure; Art. 30 records of processing; Art. 32 security** | The product's own code and docs claim GDPR-relevant features: retention sweeps labeled GDPR (`crates/aero-server/src/bin/boot/retention.rs:66`), GDPR erasure (`crates/aero-storage/src/participant.rs` delete list), personal-data export (`GET /api/me/export`), legal holds, data-lifecycle language in AGENTS.md §3 governance row | **Applicable to the extent the product processes personal data** — which this flow does (see 1.3). Jurisdiction unknown (see 1.5). |
| **Internal platform contract (contractual control)** — audit-event owned by `snaplink-audit-governance`; service→audit-governance **async-only** ingest; immutable UUID cross-system keys; event naming `<domain>.<resource>.<action>@vN` | Supplied platform-contract evidence; implemented as the outbox→relay→sink path (`AuditRelay`, `AERO_AUDIT_TOKEN_ENDPOINT`) | **Applicable (contractual)** — R-D2 extends it. |
| **Internal audit-integrity commitments** — fail-closed audit coupling ("no delete without audit record"), at-least-once delivery, at-most-once dedup, dead-row terminal state | Tree: 0239 status machine, `ON CONFLICT (event_id) DO NOTHING`, `PERMANENT_DEAD_AT=2`, PayloadGuard/ReceiptMismatch, design F1-F7 | **Applicable (self-imposed controls)** — the design's own load-bearing claims. |

### 1.3 Data inventory of the changed flow (Verified)

| Data element | Store(s) | Personal data? | Notes |
|---|---|---|---|
| `audit_events` row: workspace, actor UUID, action, target, `detail={room_id, digest}` | `audit_events` (partitioned, PK `(id, created_at)`; 0146) | **Yes** — actor/workspace identifiers are pseudonymous personal data; `digest` is message content | Already written today on user delete (Verified: `events.rs` append at :336-347; digest built at `authorization.rs:469-528`, first 120 chars of `searchable_text`) |
| `audit_governance_outbox` row: event_id, status/class/priority, 16-key payload envelope (occurred_at, actor, targets, room_id, digest, action, outcome, idempotency_key) | `audit_governance_outbox` (0239) | **Yes** — **new replication** of the same data, incl. the content digest | Proposed (FR-2/FR-3); forwarded off-box to the governance sink |
| Sink copy | snaplink-audit-governance (external system, per platform contract) | Yes | Transfer exists for the moderation lane today; R-D2 adds the message-delete class |

**Purpose of processing:** audit/governance evidence of deletion (security/legitimate-interest type purpose). No lawful-basis record, DPIA/ROPA entry, or purpose statement for the new replication exists in the tree (**Missing**).

### 1.4 Excluded / not established (do not treat as in-scope)

- **SOC 2 / ISO 27001 / ISO 27701 / PCI DSS / HIPAA / CCPA / ePrivacy / EU AI Act / NIS2**: no applicability evidence was supplied for this subsystem. **Unknown — excluded from findings.** (The tree's `retention_class='security'` is an internal label, not an attestation.)
- **Certification claims**: none exist in the inputs; nothing here certifies anything.
- **Legal holds / litigation preservation**: implemented at message level for the retention sweep only (`workspace/sweep.rs:28`); no legal-hold policy document supplied — process evidence **Unknown**.

### 1.5 Unknowns that must be resolved before any certification-style claim

- **Jurisdiction / supervisory authority / controller-vs-processor roles**: not established anywhere in the inputs. **Unknown.**
- **Data classification at deployment level**: the envelope hard-codes `data_classification='confidential'` (`crates/aero-common/src/model/audit.rs:254`) for every event regardless of actual content; whether any tenant's data is special-category (health, biometrics, etc.) is **Unknown**.
- **Where the governance sink physically resides, its operator, and its retention**: **Unknown** (vendor/subprocessor assessment not supplied).
- **Transport (TLS) and at-rest encryption** for the outbox→sink path and the Postgres stores: not evidenced in-tree (`aero-audit-connector` uses a configurable JWT token endpoint; TLS is deployment configuration). **Unknown.**
- **Framework applicability decision**: no owner decision exists on which assurance framework (if any) this product is assessed against.

---

## 2. Control matrix

Legend — status: ✅ Verified · 🟡 Partial · ❌ Gap · 🔶 Proposed (in this design, not landed) · ❔ Unknown.

| # | Requirement / obligation | Status | Repository evidence | Process evidence | Gap | Owner | Validation method |
|---|---|---|---|---|---|---|---|
| C1 | **Audit completeness — user deletes produce a durable audit event with evidence of the deletion** | ✅ today, 🔶 extended | Audit append in delete tx (`events.rs:336-347`); R-D2 adds outbox row (FR-3) | AC-1/AC-3 parity tests proposed; existing F1 rollback test stays green | None for user path; **system paths are a gap (C2)** | aero-storage (message) | `scripts/test-integration.sh` slots; F1 test measured green by QA (#8) |
| C2 | **Audit completeness — system-initiated data destruction (retention/GDPR/ephemeral sweeps) is evidenced** | ❌ Gap (pre-existing, explicitly deferred — design §9) | `sweep_expired_messages` produces no `message.deleted` token (design §9); sweep paths in `boot/retention.rs:167-169` | None | The highest-volume destruction path (retention expiry, GDPR-window deletes) writes **no audit row and no outbox row** — the opposite of what a records-of-processing regime expects for destruction events | aero-storage (workspace sweep) + governance owner | Follow-up slice: in-tx audit append + gated outbox write in sweep paths; parity drill extension |
| C3 | **Fail-closed audit coupling — no delete commits without its audit record (Art. 32 integrity; self-imposed)** | 🟡 Partial (🔶 Proposed claim unpinned) | Design F1/F2; audit append precedes writer; writer propagates `sqlx::Error` | F1 rollback test green (QA #8); **F2 writer-failure has no injection point (QA Finding 1)** | The central compliance claim "no delete-unaudited" ships without a test that can fail | aero-storage (message) | Add `delete_lane_writer_failure_aborts_delete_fail_closed` (poisoned tx + dropped-table variant) in-commit |
| C4 | **At-least-once / at-most-once delivery of the evidence to the governance sink** | ✅ Verified (machinery), 🔶 for new class | 0239 status machine 0-3; `ON CONFLICT (event_id) DO NOTHING`; `idempotency_key == event_id`; `PERMANENT_DEAD_AT=2` (relay.rs:47); AC-2b proposes set-parity + dead-row negatives | Parity drills measured green (QA #7,#10) | None material; dead rows are terminal by design — **monitoring is the gap (C7)** | aero-audit-connector | AC-2b leg; existing `l1-aggregation-drill` |
| C5 | **Storage limitation — every copy of personal data has a bounded, documented retention (Art. 5(1)(e))** | ❌ Gap (pre-existing, **amplified by this design**) | Source trail: `AERO__SERVER__AUDIT_RETENTION_DAYS` default 365 + daily partition DROP (`boot/retention.rs:24-28,173-174`; 0146). **Outbox: zero references in `boot/` — no sweep, no partition policy** (Verified by grep) | 0246: "never delete while relay runs" (design intent, not a policy) | Outbox rows — including the 120-char **message-content digest** — are retained **indefinitely**, outliving their source partition; no legal basis documented; in deployments without `AERO_AUDIT_TOKEN_ENDPOINT` rows accumulate with no consumer **and** no sweep | aero-storage (audit_governance) + ops | Status-2/3 sweep with config key (e.g., match audit window or documented legal schedule); migration + test; ROPA entry |
| C6 | **Data minimization — the audit evidence contains no more personal data than needed (Art. 5(1)(c))** | 🟡 Partial | Digest = 120-char truncation (`authorization.rs:469-528`); deleted message body is zeroed in the same tx (`blocks='[]'`, `searchable_text=''`, `embedding=NULL` — Verified `events.rs`) | None | A 120-char excerpt of a message is still personal (and possibly sensitive) content; truncation is minimization, not anonymization. The design does not justify why the digest must be replicated off-box and kept forever | aero-storage (message) | Document purpose of the digest; align its retention with C5; consider omitting/limiting it |
| C7 | **Erasure — GDPR deletion covers this data, or a documented retention basis exists (Art. 17)** | ❌ Gap | Erasure list (`participant.rs:418-430`) deliberately keeps "governance/audit records" — but `audit_governance_outbox` is keyed by event_id, not participant; its payload (actor UUID + digest) is **never** swept, never erased | None | After an Art. 17 request, the erased user's message-content digest remains in the outbox (and sink) forever, with no documented legitimate-interest/legal-obligation basis or retention schedule | Governance owner + legal | ROPA/DPIA entry with retention + erasure handling for outbox/sink copies; policy doc |
| C8 | **Access control — only authorized actors can produce the event; evidence not user-readable** | ✅ Verified (producer side) | Sender-only delete + effective write access + room membership (`authorization.rs`); no app-level audit/outbox read API (Verified: no audit route in `routes/`) | `authz_lint` CI; IDOR guards per AGENTS.md | Outbox/audit reads are DB-level only (ops/DBA credentials) — unmanaged by the app; relay bearer creds via `AERO_AUDIT_TOKEN_ENDPOINT` (JWT), rotation process **Unknown** | aero-server + ops | Pen-test/access review of DB roles; secret-rotation runbook |
| C9 | **Monitoring — failure of the audit pipeline is visible (integrity of evidence)** | ❌ Gap | `metrics_tasks.rs:169-171` monitors the **failed-pair DLQ** only; **no gauge for outbox dead rows (status 3) or backlog** (Verified) | Drills prove dead-path behavior (AC-2b) but no production alert exists | A permanently dead `message.deleted` row (PayloadGuard/ReceiptMismatch) is a silent hole in the governance feed | aero-server (observability) | Prometheus gauge (status-3 count, oldest status-0/1 age) + alert |
| C10 | **Classification & retention-class consistency** | 🟡 Partial | Envelope hard-codes `data_classification='confidential'`, `retention_class='security'` (`audit.rs:254-255`) for all events | None | `retention_class='security'` has **no implemented mapping** to any retention behavior; classification is not content- or tenant-aware | Governance owner | Document classification model + retention mapping; reconcile with C5 |
| C11 | **Legal hold interplay** | 🟡 Partial | Sweep skips legal-held messages (`workspace/sweep.rs:28`); **user delete path does not consult `legal_holds`** (Verified: no refs in `message/authorization.rs`, `events.rs`, `crud.rs`) | None | A user can delete a legal-held message today; R-D2 at least makes the event visible to the governance feed. Retention of that evidence (C5) is the only long-lived copy — currently accidental, not policy | aero-storage (message) + legal | Add legal-hold check to user delete; document preservation interplay |
| C12 | **Contractual — wire contract to the governance sink** | ✅ Verified | `AUDIT_SOURCE_SYSTEM='aero-im.source'` (`audit.rs:182`); PayloadGuard/ReceiptMismatch (`client.rs:175,212`); deployment invariant `AERO_AUDIT_SOURCE_SYSTEM` documented (design §3.5) | Drills measured green | None for the class contract; **naming**: outbound action is the local token verbatim (`message.deleted`) vs platform convention `<domain>.<resource>.<action>@vN` — pre-existing pattern (`admin.content.flag`), downstream relies on `schema_version`; not versioned | aero-audit-connector | AC-2b; document versioning decision (Info) |
| C13 | **Rolling-deploy completeness — no silent evidence loss during skew** | ❌ Gap (bounded) | Pure additive code, no schema (design §3.8) — schema-safe | QA Finding 2: old binary writes audit row but **no outbox row** during skew; reconciler (0241) is `message.moderated`-only and unchanged; no backfill | Deletes handled by an old binary are silently absent from the governance feed forever (bounded to skew window, invisible to monitoring) | Ops + aero-storage | One-shot backfill `INSERT…SELECT` documented as ops step; skew-window monitor; AC-2a negative control (orphan audit row → parity intact, no fabrication) |
| C14 | **Integrity of the produced evidence (tamper-evidence / immutability)** | ✅ Verified | `audit_events` genuinely append-only (0146 analysis); outbox rows only mutated by the relay status machine; no user-facing write path | Drills | DB-admin tampering is out of product control (standard for DB-backed logs) — note in ROPA | Ops | Access review (C8) |
| C15 | **Encryption — at rest and in transit** | ❔ Unknown | None evidenced (no TDE/app-level encryption in tree for these tables; sink transport = configured JWT endpoint, TLS not evidenced) | None | Deployment-level responsibility; must be evidenced before any assurance claim | Ops | Config/network evidence (TLS-only endpoints, encrypted volumes) |

---

## 3. Findings (severity-sorted)

Severity per `prompts/README.md` (Critical / High / Medium / Low / Info). "Regulatory risk" = GDPR exposure, since GDPR is the only regulation shown to apply; everything else is contractual/internal.

### F-1 — **High: indefinite retention of message-content digests in the governance outbox; R-D2 adds the highest-volume producer with no retention plan**
- **Evidence (Verified):** `audit_governance_outbox` is not referenced by any sweep in `crates/aero-server/src/bin/boot/` (grep) while `audit_events` is bounded to `AERO__SERVER__AUDIT_RETENTION_DAYS` (default 365) with daily partition DROP (`boot/retention.rs:24-28,173-174`; migration 0146). The outbox payload contains the 120-char digest of the deleted message's text (`authorization.rs:469-528` → envelope `payload.digest`, `outbox.rs:568-623`). The design §9 defers retention; the DB architect flags the same (M1). In deployments without `AERO_AUDIT_TOKEN_ENDPOINT` (config.rs:58), rows are never consumed **and** never swept.
- **Business/regulatory risk:** GDPR Art. 5(1)(e) storage limitation and Art. 17 erasure exposure — content of deleted messages (including of erased users, see C7) retained indefinitely off-box with no documented legal basis; retention asymmetry vs the 365-day source trail; a DPA/auditor question will be "where is the deletion record, and why does it outlive the data it describes?". Also unbounded table growth (availability).
- **Remediation:** (1) status-2/3 sweep (and age-based sweep of unconsumed rows) with a config key, defaulted to a documented retention schedule — as a tracked follow-up **blocking nothing in the design commit but scheduled before GA of this lane**; (2) ROPA/DPIA entry stating purpose, lawful basis, retention for the outbox and sink copies; (3) decision on whether the digest is needed at all (C6).
- **Evidence needed for closure:** migration + sweep test; config reference entry; ROPA entry; signed retention decision.

### F-2 — **Medium: system-initiated deletions (retention/GDPR/ephemeral sweeps) are the least-audited destruction path**
- **Evidence (Verified):** design §9 explicitly out of scope; `sweep_expired_messages` produces no `message.deleted` token; sweep paths at `boot/retention.rs:167-169`.
- **Risk:** the data-destruction events most relevant to GDPR evidence (expiry sweeps, ephemeral hard-deletes) leave no audit record at all — records-of-processing (Art. 30) and destruction-verification questions cannot be answered from the trail. R-D2 narrows the gap for user deletes, widening the relative asymmetry.
- **Remediation:** follow-up slice — in-tx audit append + gated outbox write in the sweep paths (the design's own §9 acknowledges the need).
- **Closure evidence:** parity drill extension covering sweep deletes; sample trail from a throwaway DB.

### F-3 — **Medium: no production monitoring of governance outbox dead rows or backlog**
- **Evidence (Verified):** `metrics_tasks.rs:169-171` covers only the failed-pair DLQ; no gauge/alert for status-3 dead rows or oldest-pending age. AC-2b proves dead behavior in tests, not alerting in prod.
- **Risk:** a dead `message.deleted` row (permanent PayloadGuard/ReceiptMismatch failure) silently removes evidence from the governance feed — audit completeness failure invisible to ops; under any assurance framework this is a control deficiency.
- **Remediation:** Prometheus gauge (status-3 count by class; age of oldest status-0/1) + alert; reuse existing observability sampler pattern.
- **Closure evidence:** gauge in `/metrics`, alert rule, drill asserting gauge reflects dead rows.

### F-4 — **Medium: the design's central fail-closed claim (F2) is unpinned — no writer-failure injection point (QA Finding 1)**
- **Evidence:** every existing rollback precedent fails at or before the audit append; the writer's first statement reads a provably-existing row and its INSERT can only violate hard-coded-valid CHECKs. A future refactor that swallows the writer error (e.g. `ok_or_log`) would pass every test.
- **Risk:** "no delete commits without its governance record" is the compliance heart of the change; shipping it untestable makes the claim advisory.
- **Remediation (in-commit):** `delete_lane_writer_failure_aborts_delete_fail_closed` — poisoned tx (rollback-then-call) + end-to-end `DROP TABLE audit_governance_outbox` variant asserting `Err`, `deleted_at IS NULL`, zero outbox rows, then re-create from 0239 DDL.
- **Closure evidence:** test green; existing F1 test stays green.

### F-5 — **Medium: rolling-deploy skew silently omits outbox rows with no backfill (QA Finding 2)**
- **Evidence:** design §3.8 schema-compat is verified true, but old-binary deletes write the audit row without the outbox row; reconciler (0241) is `message.moderated`-only; relay claims only existing rows. Bounded to the skew window; invisible to monitoring.
- **Risk:** completeness gap in the governance feed during every rolling deploy; not detectable post-hoc.
- **Remediation:** document a one-shot backfill `INSERT…SELECT` as an ops step; add the AC-2a negative control (orphaned `message.deleted` audit row → SUM parity holds, no fabrication) pinning the boundary.
- **Closure evidence:** drill negative control green; backfill SQL reviewed by ops.

### F-6 — **Low: hard-coded `data_classification='confidential'` and unimplemented `retention_class='security'`**
- **Evidence:** `audit.rs:254-255`; no mapping from `retention_class` to any retention behavior (ties to F-1).
- **Risk:** if a tenant processes special-category data, the classification label is wrong and obligations (e.g. heightened protection) are silently unaddressed; the label may overstate protection to auditors.
- **Remediation:** document the classification model; make retention_class a real contract (map to the F-1 sweep).
- **Closure evidence:** policy doc + config mapping.

### F-7 — **Low: user delete path does not consult legal holds (pre-existing, now observable)**
- **Evidence:** no `legal_holds` references in `message/authorization.rs` / `events.rs` / `crud.rs`; sweep path checks them (`workspace/sweep.rs:28`).
- **Risk:** deletion of legal-held evidence by a user; R-D2 at least records it, and the indefinite outbox copy (F-1) is the only surviving record — currently accidental.
- **Remediation:** add the legal-hold check to the user delete path; decide retention interplay explicitly.
- **Closure evidence:** test + policy note.

### F-8 — **Info: outbound event name `message.deleted` does not follow the platform `<domain>.<resource>.<action>@vN` convention**
- **Evidence:** writer passes the local token verbatim as outbound action (design §2.2); moderation lane precedent `admin.content.flag` (0239) also unversioned; `schema_version=1` is the versioning signal.
- **Risk:** downstream consumers keying on action strings across services may drift; low.
- **Remediation:** document the versioning decision (action verbatim + schema_version) in the wire contract.
- **Closure evidence:** contract note.

### F-9 — **Info: `occurred_at` spelling divergence between the two producers of the same table (DB architect M2)**
- **Evidence:** Rust rows format `…Z` (time-0.3.41 Rfc3339 trailing-zero trim); trigger rows spell `…+00:00`. AC-3 as specced (PG `to_jsonb` spelling assertion) would fail on writer-produced rows.
- **Risk:** downstream forensic/parse drift between moderation rows and delete rows; low but real.
- **Remediation:** AC-3 asserts semantically (auth.rs pattern); consider normalizing the writer's spelling to the trigger's.
- **Closure evidence:** AC-3 green with semantic assertion; sample row dump.

### F-10 — **Info: transport/at-rest encryption and sink subprocessor posture not evidenced**
- **Evidence:** connector = configurable JWT endpoint (`config.rs:58-71`); no TLS evidence in-tree; no TDE/app-level encryption for the stores; no subprocessor assessment for snaplink-audit-governance.
- **Risk:** cannot be certified; GDPR Art. 32 assessment incomplete.
- **Remediation:** ops evidence pack (TLS-only endpoints, encrypted volumes, sink DPA).
- **Closure evidence:** infra config + DPA reference.

---

## 4. Audit-readiness summary

### 4.1 What the repo already evidences (usable by an auditor)
- A deletion audit event exists in the source trail today, and R-D2 makes it 1:1 replicated with fail-closed coupling and at-least-once/at-most-once delivery (Verified machinery, Proposed wiring).
- Deterministic test surface: 38 `audit_governance::` tests green, parity drills green, 246-migration immutability, b5-pin guard (all **measured by QA at this revision**; must be re-run at landing).
- Access control on the producing path is sender-scoped and CI-linted.

### 4.2 Required documents (none exist in the tree — **Missing**)
1. **ROPA/DPIA entry** for the message-deletion audit flow: purpose, lawful basis, data elements (incl. the 120-char digest), recipients (governance sink), retention of **each** copy (source trail vs outbox vs sink), erasure handling. Required to close F-1/F-2/C7.
2. **Retention schedule / legal-basis note** covering outbox rows (status 2/3 and unconsumed) and the sink copy.
3. **Data-classification policy** reconciling the hard-coded `confidential` label with actual tenant data (F-6).
4. **Incident-response runbook** for audit-pipeline failure (dead rows, backlog, skew window) — ties to F-3/F-5.
5. **Vendor/subprocessor assessment** for snaplink-audit-governance and the token endpoint operator (F-10).
6. **Encryption evidence pack** (TLS in transit, at-rest protection) (F-10).
7. **Framework applicability decision** — which assurance framework (if any) the product is assessed against; currently **none evidenced** (1.4).

### 4.3 Next decisions (owner needed — not resolvable from repo evidence)
| Decision | Options | Suggested owner |
|---|---|---|
| D-1: Outbox retention | (a) status-2/3 sweep matching audit window; (b) longer documented legal schedule; (c) status quo (reject) | Governance/legal owner + aero-storage |
| D-2: Digest necessity | Keep 120-char digest vs drop vs shorten; impacts F-1/F-6 | Product + legal |
| D-3: Sweep-path audit coverage (F-2) | Fund follow-up slice vs accept gap and document | Product |
| D-4: F-4 in-commit tests | Mandatory before landing the fail-closed claim | aero-storage |
| D-5: Framework scope | Declare assessed framework(s) + jurisdiction, or keep "no certification claim" | Company/legal |

### 4.4 Landing checklist (compliance gate for the R-D2 commit)
- [ ] F-4 writer-failure tests land in-commit (poisoned tx + dropped table); F1 stays green.
- [ ] AC-2a negative control (orphan delete audit row → parity intact) lands (F-5 pin).
- [ ] AC-3 uses semantic `occurred_at` assertion (F-9) — do not land a test that fails by design.
- [ ] ROPA/retention decision (D-1/D-2) recorded, even if implementation is a tracked follow-up; design's "deferred" must name an owner and date.
- [ ] Monitoring follow-up (F-3) tracked as a work item before this lane goes GA.
- [ ] QA's Finding-5 baseline gates (truth-check exit 6, file-size exit 1) resolved or documented as pre-existing exceptions before the §8 DoD gate is claimed green.

**Bottom line:** the change is compliance-positive (it makes deletions auditable at the governance layer, fail-closed) and the wiring evidence is Verified. The blocking compliance items are **not** in the code path — they are the deferred retention decision (F-1, High), the missing failure-injection tests for the fail-closed claim (F-4, Medium), and the absence of any retention/monitoring policy for the store this design feeds (F-1/F-3). None of these require a schema change; all are cheap to close. Jurisdiction, data classification, framework applicability, and encryption posture remain **Unknown** and must be resolved by owners before any external assurance claim — repository evidence alone is not compliance.
