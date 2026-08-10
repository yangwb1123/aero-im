Compliance review written to `docs/pi-batch/sdlc-reviews/compliance_officer.md`. Summary:

## Compliance Review — Message Recall Time Window

**Scope**: config knob `AERO_RECALL_WINDOW_SECS`, two-layer window enforcement, 409 contract, metric, web surfacing. No new stores/vendors/crypto/keys/network surface.

**Applicability**: The input does not establish jurisdiction, data classification, or a formal regime. GDPR family (Art. 5(1)(e) / Art. 17 adjacency / Art. 32) is **referenced in-tree** (erasure design, legal holds, audit retention) but formally unconfirmed; SOC 2/ISO 27001/contractual controls = no evidence, excluded. Nothing is certified.

**What I verified directly** (beyond the QA/DB reviews): `.env.example:149` knob + boot log (`orig.rs:316-322`); preflight ordering (role gate → window → metric → 409, before rate charge in both transports — `handlers/messages.rs:184`, `ws/ws_impl/frame.rs:163`); the tx fence on the `FOR UPDATE` snapshot with **unchanged** UPDATE WHERE; the `message.recalled` audit row (actor/message/room/workspace/120-char digest, in-tx); redacted snapshot + blob GC; migration 0238 in full; erasure anonymizes `messages` **and** `message_edits` (Art. 17 path covers recalled content); `recalled_by` FK relies on the tombstone invariant (never fires).

**Control matrix** (14 rows): access control, no-leak, atomicity, audit trail, minimization, retention, erasure interop, operator control, observability, availability, crypto (N/A), IR (unknown), vendors (N/A), doc precision — each with repo evidence, process gap, owner, validation.

**Findings** (7): F1 Medium — non-concurrent index rebuild in 0238 (deploy-time availability, SHARE lock on hot table); F2 Low — clock-skew boundary bound; F3 Low — no committed E2E/metric regression for the window (operator control drifts silently); F4-F7 Info — no retention policy for `message_edits`/outbox rows (recall ≠ erasure: snapshot persists — needs a decision, not a fix), audit digest fragment, stale design-doc numbers, missing demotion race test.

**Audit-readiness**: technical controls are strong and test-pinned; **not yet auditable as compliant** — framework applicability, retention policy, and runbook entries are missing, and the behavioral proof (QA's live E2E) is not CI-retained. Required documents and owners listed in §4.
