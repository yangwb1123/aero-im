All evidence gathered. Here is the fix plan.

---

# Fix Plan: finish the clippy gate (`-D warnings`) — 57 remaining sites

## 0. Reproduced baseline (exact commands, actual current state)

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ PASS (working tree: 131 files modified from prior rounds) |
| `cargo clippy --workspace --all-targets -- -D warnings` | ❌ FAIL — **57 unique sites** (66 error lines incl. per-target dupes): aero-storage 30 lib + 49 lib-test, aero-eng 5+6, aero-bus 1+1, aero-signaling 1+1. Clippy 1.93.0. |
| `cargo test --workspace --lib` | ✅ PASS — 318 passed, 0 failed, 592 ignored |
| `bash scripts/web-check.sh` | ✅ PASS — 0 violations |

Note: the task premise says "aero-storage 30+49, aero-signaling 1" — actual evidence additionally shows **aero-eng (11) and aero-bus (2)** error lines; the plan covers all 57.

## 1. Root causes (file:line evidence from `/tmp/plan_clippy.log`, 57 unique sites / 24 files)

| Class | Count | Sites |
|---|---|---|
| `similar_names` | 16 | `user_blocks.rs:53/76/99` (`blocker`/`blocked`), `user_report.rs:68/224/316` (`reporter`/`reported`), `clip_collections.rs:262/271` (`clips` vs `clip1`), `call/db_tests.rs:71/460` + `call/security_tests.rs:29` (`caller`/`callee`), `notification_bundle.rs:499` (`room`/`root`) |
| `float_cmp` (strict f32/f64) | 9 | `presence.rs:215/217/222/223`, `live_presence.rs:447/449/454/455` — `assert_eq!(f64, exact literal)` in tests; `ai_context.rs:270` — `while b == a` busy-wait |
| `format_push_string` | 4 | `directory.rs:134/138/141`, `stream.rs:293` — `s.push_str(&format!(…))` |
| `clamp`-like | 4 | `channel_points.rs:459`, `hype_train.rs:306/362`, `predictions.rs:750` — `limit.max(1).min(100)` |
| `cast_sign_loss` i64→u64 | 3 | `live.rs:252/253` (`total_coins/total_qty.max(0) as u64`) |
| `cast_precision_loss` i64→f64 | 3 | `stream_viewer_sample.rs:298:17/298:30/307:37` (`sum as f64 / cnt as f64` stats) |
| `cast_sign_loss` i64→usize | 2 | `message/query.rs:307:40/307:79` (`half_window as usize`) |
| `too_many_arguments` (8/7) | 2 | `checks.rs:112` (`scan_dir`), `notification_bundle.rs:86` (`insert_many_idempotent`) |
| `manual_let_else` | 2 | `jetstream.rs:208`, `checks.rs:321` |
| `items_after_statements` | 2 | `hype_train.rs:515/516` (`const N`/`const UNITS` after statements in a test fn) |
| Single occurrences | 12 | `crud.rs:188` match→if-let · `checks.rs:292` `{cargo_path:?}` Debug fmt · `info_barrier.rs:65` pass-by-value `Row` · `signaling.rs:137` identical `Answer`/`Offer` arms · `registry.rs:75` method `add` · `stream.rs:320` items after test module · `message/mod.rs:180` empty line after doc · `checks.rs:436` `assigning_clones` · `outcome.rs:393` u128→u64 in test assert · `db.rs:55` large stack array (macro) · `cache.rs:47` u64→i64 TTL · `webhook/delivery.rs:255` case-sensitive `.ends_with(".localhost")` · `notification_bundle.rs:215` `as_secs() as f64` · `live.rs:308/309` `qty.max(0) as u32` / `coins.max(0) as u64` · `ai_context.rs` (counted above) |

Root cause: none of these are behavior bugs — all are style backlog that survived two `cargo clippy --fix` passes (non-machine-applicable or deliberately left): test-fixture naming, exact-value float asserts, SQL LIMIT clamp idioms, statistics casts, and a few API-shape nits. **Strategy per the task: fix the code, no global lint disables** — targeted `#[allow]` only where the "fix" would be wrong (documented below), matching in-tree precedent (`metrics.rs:737`, `aero-live-srt` module allows, `run_socket` too_many_arguments allow already in tree).

## 2. Module boundary & change radius (per `backend-specs/agent-guardrails.md` §2)

- **直接修改文件**: 24 files — `aero-storage`: user_blocks, user_report, clip_collections, call/db_tests, call/security_tests, notification_bundle, presence, live_presence, ai_context, directory, stream, hype_train, channel_points, predictions, live, message/query, stream_viewer_sample, message/crud, message/mod, db, cache, webhook/delivery, info_barrier · `aero-eng`: checks, outcome, registry · `aero-bus`: jetstream · `aero-signaling`: signaling
- **公共接口变化**: exactly **one** — `aero-eng::registry::CommandRegistry::add` renamed → `with_command` (lint `should_implement_trait`). Callers: `aero-cli/src/main.rs:20` and `aero-server/src/bin/aero-cli.rs:18` (macro `reg = reg.add(Box::new($c));`) + 2 test sites in registry.rs. Internal-only builder; no external consumers.
- **间接影响**: none beyond the rename (compile-time checked by workspace build). No schema/event/config/deploy changes.
- **回滚方式**: `git checkout --` the 24 files / revert the eventual commit.

## 3. Exact files/symbols to change + fix per site

**Rename fixes (similar_names, 16):** `blocked`→`target` in `user_blocks.rs` block()/unblock()/is_blocked() (+ `lock_user_block_pair` calls; SQL `blocked_id` untouched); `reported`→`target` in `user_report.rs` create_authorized/lock_active_participants/test locals; `clips`→`listed` in clip_collections.rs tests; `callee`→`target` in call/db_tests.rs (2 fns) + call/security_tests.rs; `root`→`thread_root` in notification_bundle.rs test.

**float_cmp (9):** presence.rs/live_presence.rs 8× `assert_eq!(x, lit)` → `assert!((x - lit).abs() < 1e-9)` (values are whole seconds by construction; epsilon preserves the passing assertions); ai_context.rs `while b == a` → `while b <= a` (matches the test's "increases monotonically" intent, avoids float equality).

**format_push_string (4):** directory.rs + stream.rs `push_str(&format!(…))` → `write!(s, …).expect("write to String cannot fail")` + add `use std::fmt::Write as _;` (pattern already established in scheduled_streams.rs / aero-live-hls in-tree).

**clamp (4):** `limit.max(1).min(100)` → `limit.clamp(1, 100)` in channel_points.rs, hype_train.rs ×2, predictions.rs (identical semantics).

**Casts:** live.rs 4× → `u64::try_from(x.max(0)).expect("max(0) is non-negative")` (and `u32::try_from` for qty); message/query.rs 2× `usize::try_from(half_window).expect("window is non-negative")`; cache.rs → `i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX)`; notification_bundle.rs:215 → `self.deadline.as_secs_f64()` (idiomatic, no cast); outcome.rs test → compare `u128::from(total)`; stream_viewer_sample.rs 3× → targeted `#[allow(clippy::cast_precision_loss)]` with comment (viewer counts ≪ 2^53; deliberate statistics cast); db.rs:55 → `#[allow(clippy::large_stack_arrays)]` on `migrate()` (macro-generated static, false positive); webhook/delivery.rs:255 → `#[allow(clippy::case_sensitive_file_extension_comparisons)]` with comment (hostname already `to_ascii_lowercase()`d, so the comparison is effectively case-insensitive).

**too_many_arguments (2):** targeted `#[allow(clippy::too_many_arguments)]` + one-line comment on `checks.rs::scan_dir` (private helper) and `notification_bundle.rs::insert_many_idempotent` (single caller orig.rs:968). Deviation note: the prompt suggests a context struct, but guardrails §2 (minimal changes, no drive-by refactor) + the in-tree `run_socket` precedent favor the documented allow; a struct would churn 2 call sites for zero behavior gain.

**let_else (2):** jetstream.rs:208 `let Ok(stream) = self.js.get_stream(…).await else { return Ok(None) };`; checks.rs:321 `let Some((_, ad)) = allowed else { violations.push(…); continue; };`.

**items (3):** hype_train.rs move `const N`/`const UNITS` above the statements; stream.rs move `impl From<StreamRow> for Stream` (lines 697-723) above `#[cfg(test)] mod db_tests` (line 319); message/mod.rs remove the blank line after the orphaned doc comment.

**Singles:** crud.rs match→`if let Some(r) = row { Ok(Some(r.into())) } else { … }`; checks.rs:292 `{cargo_path}` (PathBuf is Display); checks.rs:436 `stripped.clone_into(&mut name)` (clippy's own suggestion); registry.rs rename `add`→`with_command` (+2 macro call sites + tests + doc comments); signaling.rs merge `CallEvent::Answer { sdp, .. } | CallEvent::Offer { sdp, .. } => validate_sdp(sdp)`; info_barrier.rs destructure `let Row { id, workspace_id, group_a, group_b, created_by, created_at } = r;` (consumes `r`, zero call-site churn).

## 4. Test plan (in order, until green)

1. `cargo clippy --fix --allow-dirty --workspace --all-targets` (re-apply; may auto-fix a few, e.g. clone_into) — then review the diff.
2. `cargo clippy --workspace --all-targets -- -D warnings` → **0 errors** (definition of done).
3. `cargo check --workspace --all-targets` → clean (catches the registry rename and any missed call site).
4. `cargo test --workspace --lib` → 318 passed (test renames/epsilon asserts must not flip anything).
5. `cargo test -p aero-server --test authz_lint` (CI-protected).
6. `bash scripts/web-check.sh` + `scripts/truth-check.sh` + `scripts/file-size-check.sh` (AGENTS.md §4.3 — 0 violations; `row_to_model` in info_barrier still wired via `.map(row_to_model)` so truth-check stays clean).

## 5. Risk assessment

| Risk | Likelihood | Mitigation / gates |
|---|---|---|
| Epsilon asserts hide a real regression in presence math | Very low | Values are whole seconds from integer time math; current `==` asserts pass, so `(x−lit).abs() < 1e-9` is a strict superset; tests re-run. |
| `registry::add` rename breaks a call site | Low | Only 2 macro call sites + 2 tests (verified by grep); `cargo check --workspace --all-targets` catches any miss. |
| `write!` needs `fmt::Write` import (missing → compile error) | None | Compile gate catches; import added per file. |
| try_from `.expect()` panics on negative input | Very low | Every cast is preceded by `.max(0)` (provably non-negative) or is a window size; panic message documents the invariant. |
| Targeted allows hide future issues | Low | 6 allows, each with a comment explaining why the fix would be wrong (macro static array, deliberate statistics cast, already-lowercased hostname, fixed signatures); matches in-tree precedent. |
| Renames collide with an existing binding (`target`/`listed`/`thread_root`) | Very low | Compile gate catches shadowing/unresolved names. |
| Remaining-warning leak: fixes introduce new lints (e.g. `uninlined_format_args`) | Low | Iterate: run clippy `-D` after each batch until 0; CI gate `check` + test + authz_lint re-verify. |
| Gates protecting us | — | clippy `-D` (target), workspace check, 318 lib tests, authz_lint, web/truth/file-size checks — all re-run before declaring done. |

**Net change**: 24 files, ~57 mechanical edits + 1 documented public rename (`add`→`with_command`); no schema/event/config changes; fully revertible.
