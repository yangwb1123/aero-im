# Session Handoff — feature-completion sweep

**State:** 1511 lib tests / 0 fail · 149 migrations (`make migrate-smoke` 149/149 clean) ·
0 `todo!`/`unimplemented!`/`fixme` in `crates/*/src` · 0 truth-check orphans · 0 web-check
violations · `web/app.js` 989 lines (under the 1000 JS HARD line). ~64 tracked files changed
+ new modules/migrations, **uncommitted**.

This session drove the analysis backlog + ROADMAP to completion for everything buildable and
verifiable in-sandbox. Below is what shipped, how to operate it, and the four residuals that
need an external action only the operator/owner can take.

---

## 1. New migrations (0144–0149) — all additive, `migrate-smoke` clean

| # | What | Notes |
|---|---|---|
| 0144 | `stream_viewer_samples` → daily RANGE partition | zero inbound FK; DROP-PARTITION retention + maintenance fn |
| 0145 | `participant_ai_profiles` (cross-room AI persona) | participant-keyed; wired into GDPR erasure |
| 0146 | `audit_events` → daily RANGE partition | zero inbound FK, append-only; DROP-PARTITION retention |
| 0147 | `bot_subscription_deliveries` (bot webhook delivery log) | observability for the bot dispatcher |
| 0148 | `messages_partitioned` shadow table + `backfill_messages_partition()` | **additive only — does NOT touch `messages`** |
| 0149 | fix: backfill now carries the 3 MLS columns | defect found by the throwaway-DB cutover verify |

> **Not partitioned (deliberately):** `webhook_delivery_log` / `ai_jobs` / `notifications` are
> mutable state machines (`FOR UPDATE SKIP LOCKED` / read-state updates) — partitioning them
> would interact badly with concurrent updates. `messages` is the hard-STOP (see §4-i).

## 2. New env flags — **all opt-in, default OFF** (no behavior/perf change unless set)

| Flag | Enables |
|---|---|
| `AERO_NOTIFICATION_BUNDLES` | deferred reply-notification aggregation + periodic flush + sweep |
| `AERO_AUTO_MOD_RULES` | workspace auto-moderation rules (can reject messages) |
| `AERO_LOGIN_LOCKOUT_REDIS` | cross-node login-failure aggregation (needs Redis; fail-open) |
| `AERO_SPAM_GUARD_REDIS` | cross-node behavioral spam guard (3 Redis sorted-sets; fail-open) |
| `AERO_AI_CROSS_ROOM_PROFILE` | cross-room AI persona extraction + use (GDPR-erasable, transparent) |
| `AERO_CLAMAV_HOST` (+ `_FAIL_CLOSED`) | ClamAV INSTREAM upload scanning (needs a clamd daemon) |
| `AERO_PII_BACKFILL_SCAN` (+ `_SLEEP_MS`) | one-shot read-only retrospective PII scan (report-only) |
| `AERO_OTLP_METRICS` | OTLP metrics push (needs a collector; Prometheus stays default) |
| `AERO_PER_TENANT_METRICS` | `workspace` label on HTTP RED metrics (response-extension; bounded) |
| `AERO_SFU_REMB_TICK_SECS` | SFU publisher-REMB periodic tick (`0` disables) |
| `AERO_SRT_PASSPHRASE` | SRT AES-CTR encryption (SRT ingest is now spawned, like RTMP) |
| `AERO_INDEX_SIZE_SAMPLE_SECS` | index-bloat gauge sampling interval |

Existing flags reused: `AERO_LOGIN_LOCKOUT`, `AERO_SPAM_GUARD`, `AERO_PII_GUARD`,
`AERO_BLOCKED_WORDS`, `AERO_AGENTIC_ANSWERS`.

## 3. New code surface (highlights)

- **Open platform now usable end-to-end:** bot token auth (`BotTokenVerifier`), owner authz on
  bot routes, event-subscription dispatcher (`bot_dispatch.rs`) + delivery log + `GET /api/bots/:id/deliveries`.
- **Security/compliance:** ClamAV scanner (`av_scan.rs`), SAML SP (`saml.rs`, fail-closed — see §4-ii),
  retroactive PII scan (`pii_backfill.rs`), GDPR erasure extended (call transcripts, AI profiles),
  cross-node abuse aggregation, `login_failures` wired.
- **Data lifecycle:** viewer-samples + audit partitioning; messages shadow + verified cutover
  (`docs/runbooks/messages-cutover.sql`, `messages-partitioning.md`).
- **Features:** @here online-only fan-out, Markdown end-to-end (`SendMarkdown` + render spans),
  thread-mute full stack, interactive Button/Select blocks, notification inbox, group-call cap,
  screen-share frontend, attachment-content RAG, embed idempotency, prompt caching, SCIM Groups,
  per-tenant metrics, OTLP metrics export, SRT ingest.
- **Governance harness (new gates):** `scripts/truth-check.sh` (orphan modules / zero-call
  builders), `scripts/web-check.sh` (ESM syntax + import resolution), `skills/project-reorganization.md`,
  `AGENTS.md §4.2` structure rule. Frontend modularized (`web/app.js` 2297→989, 12 modules).

## 4. The four residuals — each needs an external action (not more code)

**(i) `messages` partition cutover — needs a maintenance window + your authorization.**
Shadow table + backfill are built (0148/0149); `docs/runbooks/messages-cutover.sql` is the
*verified* cutover (validated end-to-end on a throwaway DB: row parity, all 7 inbound FKs
revalidated, ON DELETE CASCADE preserved, FTS/keyset/reply-to all correct). It re-points 7 FKs
+ swaps the table — **irreversible, on live data**. Operator runs it in a window after looping
`SELECT backfill_messages_partition()` to `rows_copied=0`.

**(ii) SAML real signature validation — needs a vetted XML-DSig verifier.**
`saml.rs` is fail-closed (rejects every assertion) by design. Every pure-Rust XML-DSig crate was
tested; none is vetted (`bergshamra` compiles cleanly but is pre-1.0/unaudited). To go live:
install system `xmlsec1` + enable `samael`, or adopt `bergshamra` post-audit — then replace the
`Err(...)` in `verify_response_signature` (saml.rs:~375) with the documented verify sequence.

**(iii) OTLP metrics delivery — needs a live collector.**
Exporter is wired (`AERO_OTLP_METRICS`); point it at an OTLP/gRPC collector (`:4317`).

**(iv) SFU media E2E — needs a real WebRTC peer.**
`tick_remb` driver is wired; `SfuMediaSession` only does work after a real browser/2nd-node
ICE/DTLS-SRTP offer (no SFU SDP-offer route exists by design). Verify in a 2-browser staging session.

---

*All four are blocked on external authorization / a system dependency / real infrastructure —
not on remaining implementation. The sandbox-buildable scope is complete.*
