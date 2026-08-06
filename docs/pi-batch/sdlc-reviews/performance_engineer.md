Performance review complete: `docs/pi-batch/sdlc-reviews/performance_engineer.md`.

## Summary

**Verdict: PERF PASS.** I verified every perf-relevant claim against the working-tree diff (not just the design doc), and the feature is structurally performance-neutral.

**What I verified directly** (18-file diff, +741/−36):
- **Zero new SQL**: the tx fence (`authorization.rs:261-269`) evaluates the window on the already-`FOR UPDATE`-locked row; `created_at` was already in the locked SELECT (`events.rs:388-405`, untouched). The UPDATE WHERE fence is byte-identical.
- **Zero new locks/round-trips/serialization/caching**: only additions are 3 sub-microsecond `recall_window_expired` evals (preflight + service + tx fence) and one unlabeled counter increment on the expired path.
- **Config is boot-once** (`orig.rs:316`, sole prod call site `boot/services.rs:75`); metric is DashMap+AtomicU64, rejection-path only, zero cardinality risk; `frame.rs` and `handlers` diffs are doc-only.

**Findings** (no blockers):
- **F2 [Info, positive]**: the window check *shortens* its own failure class — expired attempts now 409 at the preflight instead of running the full ~15-17-query locked path.
- **F3 [Low, pre-existing]**: ~5 queries per doomed attempt run before `check_ws_rate_room`; WS frames unthrottled. Not a regression (same shape as edit preflight); the one new pre-rate-gate side effect is the inflatable counter → alert on rate-of-change, not absolute.
- **F4**: rejecting the SQL-predicate alternative is correct — clock mixing would break the provable inclusive boundary with zero atomicity gain.

**Key recommendations**: don't add a SQL predicate, don't drop the service-level eval, don't cache auth lookups; capture the existing `MESSAGE_PROCESSING_DURATION_SECONDS{op="recall"}` histogram as the baseline (no SLOs exist anywhere). The 5-experiment load plan is A/B against the stashed pre-feature tree with concrete acceptance rules — #1 (latency ±5%, expect ≈0 drift) and #3 (WS amplification sizing, feeds the accepted frame-budget backlog) are the load-bearing ones.

One note: `prompts/README.md` doesn't exist in this repo; I applied the shared role rules from `/home/u1/ai-batch-runner/prompts/README.md` (the batch runner's canonical copy), which is stated in the review's header.
