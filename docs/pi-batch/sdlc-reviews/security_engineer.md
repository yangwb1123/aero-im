Security review complete — written to `docs/pi-batch/sdlc-reviews/security_engineer.md` (repo design-stage convention, sibling of `async_reviewer.md`).

## Summary

**Checks performed**: `prompts/README.md` isn't in the repo — used the batch-runner copy (`/home/u1/ai-batch-runner/prompts/`) plus AGENTS.md invariants. Design is unimplemented (verified `RECALL_WINDOW` = 0 hits), so I read live code at every anchor: preflight/`recall_message` (`messages.rs:455-545`), the tx fence (`authorization.rs:206-268`), `lock_message_in_tx` (`events.rs:388`), both entry points (`frame.rs:160`, `handlers/messages.rs:164`), rate guards, client error plumbing, and all 19 `recall_outboxed_authorized` call sites.

**Verdict: PASS** — the two-layer window enforcement is oracle-safe, atomic (app-level check on the `FOR UPDATE`-locked row), rate-fair (preflight before workspace budget charge, gate S1), and injection-free. `created_at` is server-minted and never rewritten by any UPDATE (verified all four). The 409 wire contract is untouched; window-expired is unreachable by non-authors in both layers (role gate precedes it — though storage's role-before-state order differs from service's, both end with window last; a doc nuance, F8).

**Findings (all non-blocking)**:
- **F1 Medium** — `AERO_RECALL_WINDOW_SECS=0` silently disables the window with no boot-time signal; deployment templates defaulting numerics to `0` make the disabled state indistinguishable from healthy. Fix: boot log + optional gauge.
- **F2 Low** — per-instance config drift ⇒ recall rejected by instance A succeeds on B (instance shopping). Needs misconfig; make observable via the F1 gauge.
- **F3 Low** — the new counter is incrementable pre-rate-gate by an in-room author (≤20 rps, bounded) → alert distortion; document.
- **F4 Low** — inter-instance app-clock skew fuzzes the pinned inclusive boundary by ±skew (NTP is the fix; the design's DB-clock rejection still stands).
- **F5 Low** — `ImService::new` env read makes the shared db_tests fixture env-sensitive; pin `with_recall_window(ZERO)` for hermeticity (regression gate: suite green with `AERO_RECALL_WINDOW_SECS=1` exported).
- **F6–F8 Info** — cross-verified async_reviewer's findings: 19 call sites not 15 (design §9 omits `recall_index_fence_tests.rs` — will break the build as written); web msg-string discriminator needs a testable branch; §5.4 precedence list is service-layer-only.

**Abuse cases**: identity spoofing, replay, cross-tenant, and header forgery all closed (auth extractor + row-lock re-checks + participant-keyed rate limiting, no new header trust). Leakage oracles closed (window string author-only; no `created_at` re-dating path). Two policy bypasses exist **by design**: admin collusion (US3 moderation override) and window-free delete — documented as residual risks, not findings.
