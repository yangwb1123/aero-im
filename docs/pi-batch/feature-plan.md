Plan v3 complete. Both plan documents updated, no implementation code written.

## What I did this round

**1. Assessment** — same prescription: 处方档 demo(S), 工作流 L3_platform, 产品化 L2_platform_capability, 多租户信号, 0 hard rules; missing main_flow/data_source/error_path/tech_stack supplied by plan §4.

**2. Diagnosed the previous implement-stage `VALIDATION_FAILED (exit=1)` — root cause found, and it's not a code defect.** The pipeline (`/home/u1/aero-im-batch/backend-feature-pipeline.yaml`) validates the implement artifact with 5 validators. I reproduced and isolated the failure:
- `cargo-check` / `cargo-clippy` / `cargo-test` (repo-scope): **passed**
- `backendquality`: **passed** (backend-quality.json = 0 violations)
- **`completion` (`check-completion-report.py {output}`): REJECTED** — the previous round's response ended with a *prose* completion report. The checker mechanically requires a fenced ` ```yaml completion_report: ... ` block (result ∈ passed|failed|not_executed, `not_executed` entries with reasons, non-empty `commands_executed`/`changed_files`, no fabricated-pass phrasing). The temp artifact was deleted and never committed — `feature-implementation.md` still holds the stale first-round report.

**3. Verified tree state (line-by-line)** — everything from the prior rounds is in place and compiling green (`cargo check --workspace --all-targets` clean): all 5 gate fixes (events.rs dual fences, 3-col `changes_since` + index, app.js held-id funnel, SPA recall entry + 409→success, 0238 backfill reissue), all 3 closed test gaps (Recalled-arm 4-branch unit test, concurrent double-recall race, web recall tests), plus the `_lastSeen` cursor-hygiene fix in `web/ws.js`.

**4. Wrote plan v3** (no code):
- **`docs/pi-batch/feature-plan.md`** (the artifact the implementer reads) — root cause, the exact YAML template the final response must end with, and the implementer checklist: re-verify gates → write report to `docs/pi-batch/feature-implementation.md` → run `check-completion-report.py` locally until `COMPLETION: OK` → don't commit.
- **`docs/pi-batch/message-recall-plan.md` v3** — new §0.4 (validation root cause), §0.3 table with regression tests, §5.1/5.3/5.5 and §7.2 updated to "gaps closed" with test names, §8 DoD extended with **§8.5 产物契约** (artifact path, YAML template, checker hard rules, honest result grading), all stale "先写先红" references reconciled.

**Key message for the implementer round**: the code is done and green — the only thing that failed the harness was the completion-report format. The final response must literally contain the fenced YAML `completion_report:` block (the artifact text is what the validator scans), and the local `check-completion-report.py` gate must exit 0 before finishing.
