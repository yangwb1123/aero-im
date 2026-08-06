All 10 window DB tests individually verified green on a fresh migrated DB. I have everything needed for the review.

---

# QA Review — Message Recall Time Window

Reviewed: `docs/design/2026-08-06-message-recall-time-window.design.md` (v3 as-built) vs the shipped tree (`git f20e4d1` + working tree). Independent re-execution of every claimed gate plus a live end-to-end boot with `AERO_RECALL_WINDOW_SECS=5` on a throwaway migrated DB (server on :3199, dropped after).

## 1. Verdict

```
VERDICT: PASS - 13/13 design assertions verified in code; all claimed gates independently
re-executed green (check, clippy -D warnings, 2168 lib, 605 integration, 127 web,
truth/web-check, hermeticity env=1, 238 migrations); boundary/race/no-leak/precedence/
contract/web-regression coverage is real and assertion-rich; live E2E (REST+WS) reproduced
the exact 409 envelope, admin exemption, member no-leak, row immutability, and metric
semantics (aero_messages_recall_expired_total=4 exactly, 0 member probes). Remaining gaps
are Low/Info hardening items, none blocking.
```

## 2. Findings

| # | Sev | Defect pattern | Missing test / exact case |
|---|---|---|---|
| F1 | Low | Ordering invariant only code-inspected for the *window* variant of gate S1. The service test `recall_preflight_window_expired_author` comments "Preflight rejects BEFORE any rate charge" but the charge (`check_ws_rate_room`) lives in the handlers — the service test cannot observe it. The behavioral proof (`smoke_recall_rate_gate.py`) covers the role-gate variant only. | Deterministic ledger smoke (mirror `smoke_recall_rate_gate.py` style, no sleep needed): `AERO_WS_RATE_STANDARD_PER_MIN=3`; author sends m1; backdate `created_at` past window (SQL, app-clock param); author recall ×4 → each 409; author sends m2 → 200 (budget survived = conservation proven); author recall m2 → 429 + Retry-After; WS attempt → error frame `code=rate_limited`. |
| F2 | Low | No committed E2E for the window feature itself (window-expired 409 over real REST/WS is pinned only at unit/contract/mock layers). I verified it live (all 6 checks passed), but CI has no script that would catch a future handler reorder or WS-envelope drift. | `scripts/smoke_recall_window.py` (booted server, `AERO_RECALL_WINDOW_SECS=5`): send → sleep 6s → REST recall = exact `409 {"code":"conflict","msg":"conflict: recall window expired"}`; row still live; channel-admin recall → 200; member probe → 403 without window string; WS `recall_message` → error frame `conflict: recall window expired`. |
| F3 | Low | Metric emission has zero committed test (repo-wide convention, but the design sells the counter as the US4 tuning signal). Nothing asserts the emit site, count-per-rejection, or author-only scope — a refactor that moves/drops the emit passes CI. | Service or handler test: after one preflight rejection assert `aero_messages_recall_expired_total` incremented by exactly 1; member probe → no increment; WS-frame rejection → increment. Requires a small metrics-assert harness (`metrics::gather`-style; none exists today). |
| F4 | Info | Honesty-labeling imprecision: design §6 attributes "preflight-before-rate-charge ordering" to the service DB test, which cannot observe the charge (see F1); actual evidence = code inspection + pre-existing live smoke. Direction is honest, scope overstated. | — |
| F5 | Info | Design gate result says "~1100 lib tests"; actual re-run = 2168 passed, 0 failed. Undercount in the safe direction (all green stands); number should be refreshed. | — |
| F6 | Info | Race test description claims "one outbox row" but the test asserts only the winner partition; the outbox count is a consequence, not an asserted quantity (no `event_outbox` query). | Add to `recall_boundary_race_author_vs_admin`: `SELECT count(*) FROM event_outbox WHERE message_id=$1` → exactly 1 after both futures resolve. |
| F7 | Info | Concurrent demotion mid-recall has no race test (role re-checked under row lock is code-inspected; the send path has `send_rechecks_admin_role_after_waiting_for_demotion` at `authorization_tests.rs:197`, recall has none). Pre-existing gap pattern, not introduced here. | `recall_race_author_demoted_before_commit`: `tokio::join!` recall vs role-demotion; assert demoted actor → Forbidden and zero writes. |

## 3. Risk-coverage matrix

| Risky path | Covered | Evidence |
|---|---|---|
| Boundary precision `t=window` inclusive | ✅ | Unit with single captured `now` (`messages.rs:769`) + storage margins 86399/86401 (`recall_tests.rs:845`) |
| `0` = unlimited / invalid env → default | ✅ | Unit table (unset/0/60/trim/garbage/negative/overflow) + storage 30-day ZERO |
| Env → service wiring | ◐ | Boot log verified live (`recall_window_secs=5`); no env-mutation test (documented parallel-safety choice) |
| Race: author-expired vs admin | ✅ | `tokio::join!`, order-agnostic partition assert, no sleeps |
| Duplicate submit (double recall) | ✅ | Pre-existing `concurrent_double_recall_has_exactly_one_winner` + already-recalled 409 |
| Precedence (deleted → recalled → role → window) | ✅ | Storage + service tests; precedence chain pinned in `docs/recall-window.md` |
| Permission changes / demotion mid-recall | ◐ | Role re-checked under row lock (code); no dedicated race test (F7) |
| 409 no-leak to non-privileged | ✅ | Storage + service tests + my live member-probe check (403, no window string) |
| 409 wire contract (REST exact body) | ✅ | `error.rs:68` pins `{"code":"conflict","msg":"conflict: recall window expired"}` |
| WS error envelope | ✅ | Contract via shared envelope; live-verified error frame |
| Web 409-swallow regression (the historical bug) | ✅ | `recall_errors.test.js` + `api.test.js` real-envelope; window-expired → info toast, convergent 409s → silent, 429/network/403/404/undefined mapped |
| Optimistic-rollback / unmount | N/A | Web recall is not optimistic (placeholder arrives via `msg:recalled`); pure fn, no DOM |
| Row immutability after rejection | ✅ | Service test asserts `recalled_at IS NULL` + live check |
| Partial success / outbox integrity | ◐ | Tx all-or-nothing + one-winner partition; outbox count not directly asserted (F6) |
| Timezone/clock mixing | ✅ | Both layers app-clock `now_utc()`; backdating is app-clock parameter-bound, never DB `now()` |
| Timeout-uncertain | N/A | No new timeouts; client status-0 → error toast tested |
| Property/fuzz | N/A | No money/parser surface; parse edge cases table-tested |

## 4. Honesty audit (claimed vs executed)

| Claimed (design §1/§6 gate results) | My re-execution | Verdict |
|---|---|---|
| `cargo check --workspace --all-targets` clean | ✅ 0 errors | **Executed, passed** |
| `cargo clippy --workspace --all-targets -- -D warnings` clean | ✅ 0 warnings | **Executed, passed** |
| `cargo test --workspace --lib` ~1100 green | ✅ 2168 passed / 0 failed (count stale-low, direction safe) | **Executed, passed** |
| `scripts/test-integration.sh` 605/605, throwaway DBs dropped | ✅ 605/605 in 42s; DB dropped | **Executed, passed** |
| `node --test web/*.test.js` 127/127 | ✅ 127/127, 0 skipped | **Executed, passed** |
| `scripts/web-check.sh` 0 violations | ✅ 0/64 files | **Executed, passed** |
| `scripts/truth-check.sh` 0 orphans | ✅ 0 ORPHAN (3 UNWIRED = allowlisted builders) | **Executed, passed** |
| Hermeticity regression with `AERO_RECALL_WINDOW_SECS=1` | ✅ full lib suite green | **Executed, passed** |
| Migrations 238 before/after, zero new | ✅ 238 in tree, no migration in diff | **Executed, verified** |
| 10 window DB tests green (6 storage + 4 service) | ✅ re-ran all 10 individually on fresh migrated DB | **Executed, passed** |
| Boot log `recall_window_secs`/`recall_window_unlimited` | ✅ live: `recall_window_secs=5 recall_window_unlimited=false` | **Executed, verified** |
| Metric emitted only at preflight, both transports | ✅ live `/metrics`: exactly 4 (my 2 REST + 2 WS author-expired attempts), 0 member probes, `recalled_total=3` | **Executed, verified** |
| Live E2E window behavior (not claimed in design, I added it) | ✅ 6/6 checks: 200 in-window / 409 exact / row live / admin exempt / member 403 / WS frame | **Executed, passed** |
| `scripts/smoke_recall_rate_gate.py` (pre-existing) | ⏸ Not run — needs booted server with tiny-budget env; not claimed in this round's gate results; role-variant of S1 verified by code inspection | **not_executed (no claim made)** |
| Browser-level E2E | ⏸ No browser env; none claimed (web logic isolated in pure module per repo convention) | **not_executed, honestly absent** |

No "理论上应该通过" phrasing found; deviations (predicate in `aero-common`, `from_sink` ZERO pin, dual boot-log fields) are documented with build-time evidence (E0063); `#[ignore]` DB tests are DATABASE_URL-gated per repo convention and are actually executed by the integration script — not skipped to pass the pipeline.

**Bottom line**: implementation matches the v3 as-built design at every anchor I checked (predicate, preflight ordering, tx fence, unchanged UPDATE WHERE, 409 contract, metric, config plumbing, fixture hermeticity); every verification claim survived independent re-execution plus my own live E2E. F1–F3 are worthwhile hardening items; none block release.
