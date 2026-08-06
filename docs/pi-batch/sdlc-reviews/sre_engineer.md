SRE review complete: `docs/pi-batch/sdlc-reviews/sre_engineer.md`.

## Summary

**Verdict: OPS PASS with hardening — no launch blockers.** I verified every operational claim against the working tree (`4f44552` + diff), re-ran the `parse_recall_window` unit test locally, and applied the shared role rules from `/home/u1/ai-batch-runner/prompts/README.md` (this repo has no `prompts/README.md`, same as the other reviewers found).

**Key verified facts**
- Feature is **config-only at deploy**: plain env `AERO_RECALL_WINDOW_SECS` (`.env.example:149`), read once at `ImService::new` with a boot log distinguishing `0`-vs-fallback; 238 migrations unchanged this batch; zero new boot surface.
- Only hard dependency = PG (already in `/health/ready`); Redis rate gate fail-open; NATS fan-out failure converges via the 250ms outbox relay; all failure paths convergent (no retry storms, no zombie states — F6 positive).
- Misconfiguration fails **restrictive** (garbage → 86400s), with `"0"` = unlimited as the single fail-wide value, distinguishable in the boot log.
- `MESSAGE_PROCESSING_DURATION_SECONDS{op="recall"}` baseline already exists (messages.rs:602).

**Findings (no blockers)**
- **F1 Medium** — the US4 tuning signal is scrape-only: both recall counters are unregistered (no HELP, absent until first use) and unlabeled; no alert rules or runbook exist in the repo. An operator typo (`36000` for `3600`, or `=1`) gets zero alerts.
- **F2 Low** — WS preflight pump inflates the metric unboundedly (accepted, doc-corrected); alert thresholds must tolerate it.
- **F3 Low** — mixed-version deploy: 0238's non-concurrent index DROP+CREATE + old binaries' `changes_since` can't see recalls (stale display, self-heals).
- **F4 Low** — window changes require a rolling restart that transiently mixes enforcement windows.
- **F5 Low** — recalled content is terminal and backup-agnostic (RPO = infinite); only the redacted `message_edits` snapshot + audit digest survive.

**Deliverables per the prompt**: service/dependency map + assumptions (§1), readiness table covering signal/dependency/failure/alert/runbook (§2), severity-ordered findings with recovery validation (§3), six failure drills — outage, saturation, bad rollout, stale state, restore, feature rollback (§4), and launch blockers/rollback triggers/monitoring gaps/residual risks (§5). Rollback is config-only (`=0` + rolling restart); monitoring gaps include the dormant OTLP metrics mirror (production wires traces only) and the absence of any SLO/RTO/RPO values repo-wide.
