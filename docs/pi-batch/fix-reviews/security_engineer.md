Security review complete — written to `docs/pi-batch/fix-reviews/security_engineer.md`. I independently traced every changed security-relevant line (both entry points, both preflights, the storage recall transaction, all four content writers, the backfill query, the web client, the new tests) and re-ran `cargo check -p aero-server --all-targets` (clean, 20.5s).

## Verdict: **FAIL** — one blocking finding

**S1 (Medium, blocking) — the F3 gate itself is a workspace-wide DoS amplifier.** Verified by code trace, not assertion:

- `assert_message_recall_preflight` (`messages.rs:452-476`) checks room access + deleted + recalled, but **omits the recall-role gate** (`recall_authorized`), which runs only later inside `recall_message` (`messages.rs:509-512`).
- Both entry points (WS `frame.rs:163-166`, REST `messages.rs:182-186`) charge `check_ws_rate_room` **between** the preflight and the role gate, and `check_ws_rate` increments the cluster-wide Redis counter unconditionally (`ws_rate.rs:293`).
- Exploit: any non-author room member fires `RecallMessage` at other members' messages → preflight passes → budget charged → 403. WS frames have **no per-frame limiter** (only HTTP routes get the per-client 20 req/s middleware), so a single socket drains the shared 1200/min workspace budget (send/edit/history/search/blob all share it) in seconds — repeatable every window.
- Violates F3's own documented "same gate shape as EditMessage": edit's preflight includes its author gate (`editable_message` → sender check); recall's doesn't. Fix: hoist `role_of` + `recall_authorized` into the preflight; regression test = plain member → `Forbidden` from preflight + a budget-conservation assertion.

## Other findings

- **S2 (Low)** — 429 from the new gate is unmapped in the web client (`app.js:668` special-cases only 409), and `ws_rate` rejections carry no `Retry-After` (only `rate_limit.rs` middleware sets it).
- **S3 (Info)** — pre-existing cross-workspace 404-vs-403 existence oracle; negligible (unguessable UUIDs), codebase-wide convention, state checks correctly post-access.
- **S4 (Info)** — defense-in-depth SQL fence missing on the two transactional writers (lock+re-check is correct today); plus `transcribe_bot.rs:7` doc drift.

## Positive controls verified
F1 fence inventory complete (3 lockless SQL fences + 2 lock+re-check + recall's own one-shot WHERE + backfill fence — no uncovered `UPDATE messages` writer); oracle-safe error ordering and TOCTOU closure under row locks; redacted history snapshot + blob GC reference-scan; hermetic authz_lint with `matched >= 2` self-check; test-isolation fix correct. Matches the async_reviewer's finding 1 (I confirmed it independently against the actual charge order), and adds the WS-unbounded-frame detail that raises the drain rate, plus the abuse-case table and prioritized validation plan.
