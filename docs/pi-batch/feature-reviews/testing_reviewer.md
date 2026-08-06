All verification complete. Working tree restored (only the feature's 59 files), throwaway DB dropped. Here is the review.

---

# Testing Review — Message Recall (消息撤回)

## Verdict

**VERDICT: FAIL - two blocking test gaps, both reproduced with failing tests: (1) system edits (`edit_outboxed_system`, unfurl_bot path) resurrect recalled content into the placeholder — original blocks + FTS index restored; (2) `changes_since` (change-replay/resync) never returns recalled messages, so clients offline during the recall keep the original content indefinitely. The suite covers the happy path, permission matrix, and failure ordering well, but misses exactly the two at-least-once / async-side-effect races the feature is most likely to fail on.**

Reproduction (probe tests written against the feature code, run on a throwaway migrated DB, then removed):

```
probe_system_edit_after_recall_does_not_resurrect_content ... FAILED  (edit_outboxed_system returned Some on a recalled message)
probe_changes_since_includes_recalled_messages ... FAILED  (got 0 rows)
```

## Findings table

| # | Sev | Defect pattern | Missing test | Exact test case that catches it |
|---|---|---|---|---|
| 1 | **P1** | Timeout-uncertain async side effect re-enters a terminal state: `edit_locked_outboxed_in_tx` (`message/events.rs`) guards `deleted_at` + `version` but **not** `recalled_at`. `unfurl_bot` holds event-time `msg.blocks`, does a slow network fetch, then re-reads `get_version` (post-recall) and calls `edit_outboxed_system` — the UPDATE passes (`version` matches, `deleted_at` NULL) and writes the original content + preview cards back into the recalled message, re-indexing it into FTS (`searchable_text` recomputed). User edit path is protected only incidentally by stale `expected_version`. | Storage: recall → system edit must be a no-op. | `recall_outboxed_authorized(id, author)` → `get_version` → `edit_outboxed_system(id, original_blocks, current_version, None, None)` → assert `Ok(None)`, stored blocks == placeholder, `version` unchanged. **Fails today** (returns `Some` with original content). |
| 2 | **P2** | Partial-success convergence gap: recall sets `recalled_at` but never `edited_at`, while `changes_since` (`message/query.rs`) keys on `GREATEST(edited_at, deleted_at)`. A client that misses the live `Recalled` frame (offline, reconnect, `replayChanges`/`resync`) never receives the mutation — the server-side replay never emits it, even though the web client's `applyChange` already handles `m.recalled_at`. Recall's core promise (content removed from all viewers) doesn't converge. | Storage: change-replay includes recalls. | Insert message, set cursor `since = now - 1min`, `recall_outboxed_authorized`, then `changes_since(room, since, 50)` → assert the message appears with `recalled()`. **Fails today** (0 rows). Fix: `GREATEST(edited_at, deleted_at, recalled_at)` (or set `edited_at` on recall) + regression test. |
| 3 | P3 | Race coverage absent: no concurrent recall/recall, recall/delete, or recall/edit tests. Code is defensible (row lock + `WHERE recalled_at IS NULL` + version check), but the double-recall race path (`Conflict` vs `Ok(None)`) is untested. | Storage: two concurrent recalls. | `tokio::join!` two `recall_outboxed_authorized(id, author)` on a fresh message → exactly one `Some`, the other `Conflict("already recalled")`; stored blocks == placeholder, `version == original + 1`, exactly one outbox row. |
| 4 | P3 | Relay suppression branch untested: `materialize_outbox_payload` `Recalled` arm (delayed event suppressed when a later mutation bumped version) has no unit test, unlike the pure-function coverage the arm deserves. | Unit (`aero-im-core/src/service/outbox.rs`). | Build `EventOutboxRow` kind=Recalled with version V, live message version V+1 → assert `None`; live version == V → assert payload is `Recalled` with current blocks. |
| 5 | P3 | Test isolation debt (pre-existing convention, not feature-specific): `aero-im-core/src/db_tests/recall_tests.rs` never cleans up participants/workspaces (relies on ULID uniqueness). No sleeps anywhere — timing control is clean. | — | Not blocking; matches existing db_tests convention. |

## Risk-coverage matrix

| Risky path | Covered? | Evidence |
|---|---|---|
| Author recall happy path (placeholder, version bump, audit, outbox, history snapshot, GC enqueue) | ✅ | storage `author_recall_replaces_content_and_records_audit_in_one_tx` + im-core `recall_success_returns_placeholder_and_author_flow` (both run & pass) |
| Permission matrix: author / owner / admin / plain member / stranger | ✅ | pure `recall_permission_matrix` + storage `recall_permission_matrix_author_admin_owner_member` + im-core admin/member/stranger tests |
| Stable failure ordering: 404 → 403 → 409 deleted → 409 recalled → 403 role | ✅ | im-core db_tests (all three) + double-recall/deleted Conflict cases |
| Multi-tenant isolation (cross-workspace / cross-room, no state oracle) | ✅ | storage `recall_cannot_cross_workspace_boundaries` + im-core `recall_cross_room_member_is_forbidden_without_state_leak` |
| Delete-after-recall (orthogonal transitions) | ✅ | storage `recalled_message_can_still_be_deleted` |
| Migration 0238 (both tables + outbox CHECK) | ✅ | storage `recall_schema_columns_and_outbox_kind_are_applied` |
| WS frame contract (`type:"recalled"`, seq, placeholder, distinguishable from `edited`) | ✅ | `recalled_frame_shape_carries_placeholder_message` |
| Event wire round-trip + additive serde compat (pre-recall JSON) | ✅ | `recalled_event_roundtrips_and_fans_to_room`, `message_deserializes_without_recall_fields` |
| Embedding backfill re-embedding the placeholder | ✅ (by pre-existing guard) | `list_without_embedding` requires `searchable_text <> ''`; AiWorker `should_skip_embed` no-ops |
| AI context purge on Recalled | ✅ | `bus.rs` matches `Recalled` in purge set |
| **System edit after recall (unfurl) resurrecting content** | ❌ **FAIL** | probe test above; `edit_locked_outboxed_in_tx` lacks `recalled_at IS NULL` |
| **Change-replay/resync delivering recalls to offline clients** | ❌ **FAIL** | probe test above; `changes_since` misses `recalled_at` |
| Duplicate submit (double recall → 409) | ✅ sequential; ❌ concurrent | sequential Conflict tested; no concurrent-race test |
| Recall vs delete / recall vs edit concurrent races | ❌ | no tests (code appears safe via locks/version) |
| Fast-dispatch failure → durable relay materialization for Recalled | ⚠️ partial | code mirrors Edited; no unit test of the Recalled arm |
| Real-browser E2E recall UI | ⬜ not executed | honestly listed in `not_executed`; render.js path + web-check static gate only |

## Honesty audit (completion-evidence)

All six claimed commands were **re-executed and verified** — none fabricated:

| Claimed | Verified |
|---|---|
| `cargo check --workspace --all-targets` | ✅ passed (re-ran) |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ passed (re-ran) |
| `cargo test --workspace --lib` | ✅ passed (re-ran; 0 failed across all crates) |
| `bash scripts/web-check.sh` | ✅ passed (0 violations, 56 files) |
| `bash scripts/truth-check.sh` | ✅ 0 violations (3 UNWIRED are the allowlisted infra seams per AGENTS.md §4.4) |
| `bash scripts/test-integration.sh` | ✅ passed (re-ran, exit=0, 590 ignored tests incl. all 8 recall DB tests on a fresh throwaway DB) |

- `not_executed` (real-browser E2E) is listed with an honest reason; client coverage is bounded by the frame contract test + render.js placeholder path, which is a fair statement.
- No "理论上应该通过" phrasing; no skipped-to-pass tests (`#[ignore]` DB tests genuinely run green under `test-integration.sh`).
- Residual risks (terminal recall, no time window) are honest product decisions, not hidden gaps.
- **Honesty gap**: the report's claim "storage transaction re-checks access/role/state under row locks" is true for the *recall* path itself, but the *system-edit* path that shares the recalled row has no state re-check — the report's "与既有编辑/删除同一不变量" (same invariant as existing edit/delete) assertion does not hold for `edit_outboxed_system`, and the missing test is what would have surfaced it. The completion report overstates convergence ("all clients render the placeholder") given finding #2.

**Bottom line**: the implementation is well-tested on its happy path, permission matrix, and transactional contents, and every claimed command is real. But the two reproduced defects (P1 content resurrection via unfurl's system edit; P2 change-replay never delivering recalls) violate the feature's core promise — recalled content must stay gone for every viewer. Both need a failing-test-first fix before this can pass.
