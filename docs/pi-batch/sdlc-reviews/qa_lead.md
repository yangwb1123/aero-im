QA review complete — written to `docs/pi-batch/sdlc-reviews/qa_lead.md`. **VERDICT: PASS** — no blocking defects; every acceptance criterion is backed by tests I re-ran on the working tree.

## What I actually ran (all green)

| Gate | Result |
|---|---|
| `cargo test --workspace --lib` | **2168/0** (678 DB-gated ignored) |
| Hermeticity (`AERO_RECALL_WINDOW_SECS=1` exported) | **2168/0** |
| `scripts/test-integration.sh` (throwaway DBs) | **605/0** — all 13 new recall-window DB tests individually `ok` |
| `node --test web/*.test.js` | **127/0** (recall-specific: 17/0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `web-check.sh` / `truth-check.sh` | 0 violations / 0 orphans |

## Key verification results

- **13 new tests** (2 unit + 6 storage DB + 4 service DB + 1 server contract) + 6 web tests + 19 updated call sites. Assertions are genuinely order-agnostic where needed (race test proves the partition, not the order — author's path never writes, so admin-always-wins is stable), latency-proof margins (86399/86401s vs ms drift), app-clock parameter-bound backdating (no DB `now()` mixing), and row-scoped mutations.
- **Gate S1** (preflight before rate charge) is structurally pinned by `authz_lint.rs` (preflight → `check_ws_rate_room` → mutation order enforced at source level), not just by inspection.

## Findings (all bounded)

- **F1 Medium** — `recall_errors.js` discriminates by blacklist: any *non-window* 409 → silent `null`. Today's literals are all convergent (correct), but a **new/reworded 409 variant is silently swallowed**. Fix: invert to whitelist the convergent literals; exact test provided.
- **F2 Low** — no service-level test of in-window author success through the non-zero-window path.
- **F3 Low** — owner-recalling-*own*-expired-message gets 409 (window applies to sender even if owner/admin); docs wording is ambiguous vs implementation, and no test pins the contract.
- **F4 Low** — dead-session recall doesn't trigger `forceReauth` (repo convention does).
- **F5 Low** — metric emit (`aero_messages_recall_expired_total`) untested; confirmed single-sited (`messages.rs:525`); needs a capture harness (design's accepted backlog).
- F6/F7 Info — dormant WS recall frame path; `"+60"`/`"-0"` parse edge.

## CI gaps

No automated HTTP-layer E2E, no metric-assert harness, no demotion-race test — all in the design's accepted backlog and non-blocking (contract test + authz_lint + layer tests mitigate). Flake risk assessed as low across all new tests. Exit criteria: F1–F4 recommended before ship; F5/E2E can ride the existing backlog.
