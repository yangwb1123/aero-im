All evidence gathered. Here is the complete fix plan.

---

# Fix Plan: aero-common clippy gate (`-D warnings`)

## 0. Reproduced baseline (exact commands, run against /home/u1/aero-im)

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ PASS (9.45s) |
| `cargo clippy --workspace --all-targets -- -D warnings` | ❌ **FAIL** — `aero-common` lib: **16 errors**, lib-test: **29 errors** |
| `cargo test --workspace --lib` | ✅ PASS — 318 passed, 0 failed, 592 ignored |
| `bash scripts/web-check.sh` | ✅ PASS — 0 violations |
| `scripts/truth-check.sh` / `scripts/file-size-check.sh` | ✅ PASS (0 orphans / 0 violations) — extra AGENTS.md §4.3 gates |

The 16 vs 29 split is fully explained: **29 unique lint hits**; 13 of them live inside `#[cfg(test)] mod tests` of `markdown.rs`, so the lib target reports only the 16 non-test hits and the lib-test target reports all 29. The "17 errors" seen when linting other crates (`cargo clippy -p aero-server …`) are just aero-common's errors **propagated through the dependency graph** (verified: same 16 aero-common locations). `aero-push` (no aero-common dep) shows 0. **The entire gate failure is confined to `crates/aero-common`.**

## 1. Root causes (file:line evidence from `/tmp/clippy_full.log`)

**8 lint classes, 29 hits, 7 files — all pre-existing style backlog, no single regression:**

| Class | Count | Sites (clippy 1.93.0 evidence) |
|---|---|---|
| `assertions_on_constants` | 13 | `markdown.rs:247,260,272,284,297,300,314,352,365,380,391,403,416` — all `assert!(false, "…")` failure branches inside `#[cfg(test)] mod tests` (starts line 241) |
| `doc_markdown` | 8 | `markdown.rs:9` (`CommonMark`), `:22` (`snake_case`), `:30` (`ParticipantId`); `mls.rs:29` (`KeyPackage`); `model/mod.rs:3` (`REFACTOR_PLAN.md`); `model/event.rs:85` (`ULID`), `:87:44` (`delivery_id`), `:87:57` (`participant_id`) |
| `match_same_arms` | 2 | `model/block.rs:146` — `Text\|Code => Some(content)` vs `Thought { hidden: false } => Some(content)`; `model/event.rs:179` — 12 flat `room_id` variants vs `Call(Invite\|End\|Caption\|Join\|Leave\|SfuPublisher)` both `Some(*room_id)` |
| `cast_possible_truncation` | 2 | `markdown.rs:206-207` — `r.start as u32`, `r.end as u32` (RawSpan is `usize`, `Span.start/end` are `u32` wire-format, `model/block.rs:15`) |
| `derivable_impls` | 1 | `model/media.rs:89` — manual `impl Default for CallMode` returning `Self::P2p` |
| `result_large_err` | 1 | `config.rs:283` — `pub fn load() -> Result<Self, figment::Error>`; `figment::Error` ≥ 208 bytes |
| `redundant_closure` | 1 | `markdown.rs:45` — `.flat_map(|line| parse_line(line))` |
| `items_after_statements` | 1 | `markdown.rs:92` — `struct RawSpan` declared after `let bytes`/`let len` statements inside `parse_spans` |

**Why they exist:** historical doc style (words like `snake_case`/`ULID` written without backticks), pre-derive-Default era manual impl, `assert!(false)` as an old test-failure idiom, and deliberate `usize→u32` narrowing for the `Span` wire format. CI comment in `.github/workflows/ci.yml` (job `check`) confirms the repo knowingly carries a clippy backlog: *"The workspace intentionally carries a documented clippy backlog; the repository gates prohibit new warnings rather than pretending the existing backlog can be promoted to hard errors."* This task = eliminate the aero-common slice of that backlog.

## 2. Module boundary & change radius (per `backend-specs/agent-guardrails.md` §2)

- **直接修改文件**: 7 files, all under `crates/aero-common/src/`:
  `markdown.rs`, `config.rs`, `mls.rs`, `model/mod.rs`, `model/block.rs`, `model/event.rs`, `model/media.rs`
- **间接影响模块**: none — `aero-common` is the leaf crate; no runtime behavior changes anywhere
- **公共接口变化**: **none** (config.rs keeps `Result<Self, figment::Error>` signature; `CallMode` keeps identical `Default` semantics; match-arm merges are semantically identical bodies)
- **数据库/事件/配置/部署变化**: none. **回滚方式**: revert the single commit / `git checkout` the 7 files.

## 3. Exact files/symbols to change

1. **`markdown.rs`** (20 hits — bulk of the work)
   - 13× `assert!(false, …)` → `panic!("…")` in `mod tests` — keep every message string verbatim (`"expected text block, got {other:?}"`, `"expected Link, got {other:?}"`, `"expected text block"`, `"expected text"`). Idiomatic, identical runtime semantics (both panic; `assert!` is not compiled out in release).
   - L45 `.flat_map(|line| parse_line(line))` → `.flat_map(parse_line)` (eta-reduction).
   - L92: move `struct RawSpan` to the top of `parse_spans`'s body (before `let bytes`/`let len`); keep the `#[derive(Debug)]` and comments.
   - L206-207: `#[allow(clippy::cast_possible_truncation)]` on the `.map(|r| …)` closure (or the fn) with the in-crate precedent comment style (`metrics.rs:737` uses exactly this allow). Rationale: `Span` is u32 by wire-format design; `try_from` would add error handling to an infallible path and `unwrap_or(u32::MAX)` would silently corrupt offsets.
   - L9, L22, L30 doc comments: add backticks — `` `CommonMark` ``, `` `snake_case` ``, `` `ParticipantId` ``.
2. **`config.rs`** L283: `#[allow(clippy::result_large_err)]` + one-line comment on `AppConfig::load()`. Recommended over `Box<figment::Error>`: keeps the public signature; zero caller churn (verified callers: `aero-server/src/bin/main.rs:31`, `aero-cli.rs:64`, `aero-cli.rs:559` all use anyhow `.context()`/`is_ok()` which would still compile with Box since `figment::Error` is Send+Sync — `Tag` is `u64`, all fields are — but the allow is the minimal, risk-free option).
3. **`mls.rs`** L29: `` `KeyPackage` `` backticks.
4. **`model/mod.rs`** L3: `` `REFACTOR_PLAN.md` `` backticks.
5. **`model/block.rs`** L146: merge arms — `Self::Text { content, .. } | Self::Code { content, .. } | Self::Thought { content, hidden: false } => Some(content)` (guard `hidden: false` preserved; binding types identical).
6. **`model/event.rs`** L179: merge `RoomEvent::Call(CallEvent::Invite { room_id, .. } | CallEvent::End { room_id, .. } | CallEvent::Caption { room_id, .. } | CallEvent::Join { room_id, .. } | CallEvent::Leave { room_id, .. } | CallEvent::SfuPublisher { room_id, .. })` into the flat `Some(*room_id)` arm (per clippy's own suggestion); `Call(Answer|Ice|Roster|Offer) => None` arm untouched. L85/87: backticks on `ULID`, `delivery_id`, `participant_id`.
7. **`model/media.rs`** L89: remove manual impl; add `Default` to the existing `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]` and `#[default]` on `P2p` (MSRV 1.80 ≫ 1.62 required; serde `rename_all` unaffected).

**Explicitly NOT changed**: root `Cargo.toml` lints, `web/`, any other crate, any `#[allow]` at crate level (targeted allows only, matching house style).

## 4. Test plan (in order)

1. `cargo clippy -p aero-common --all-targets -- -D warnings` → **0 errors** (the crate-scoped gate; this is the true definition of done).
2. `cargo check --workspace --all-targets` → clean.
3. `cargo test --workspace --lib` → 318 passed, 0 failed — the 13 touched `markdown.rs` parser tests exercise the changed `panic!` branches' happy paths; failure branches are unreachable in green runs.
4. `cargo clippy --workspace --all-targets` (no `-D`) → aero-common contributes **0 warnings** (29 fewer); verify no new warnings anywhere.
5. `bash scripts/web-check.sh`, `scripts/truth-check.sh`, `scripts/file-size-check.sh` → unchanged (0 violations).
6. `cargo test -p aero-server --test authz_lint` — CI-protected source lint; unaffected but part of AGENTS.md §4.3 提交前必过.

## 5. Risk assessment

| Risk | Likelihood | Mitigation / gates |
|---|---|---|
| **Workspace-wide `-D warnings` still red after fix** — after aero-common, the gate will fail on the next backlogged crate (`aero-storage` ~203 warnings, `aero-server` ~200, `aero-cli` 40, … ~680 total). | Certain | This is the documented, accepted backlog (CI runs clippy **without** `-D warnings`; AGENTS.md §4.3 requires "别新增警告" not "零 backlog"). Must be stated honestly in the PR: green = crate-scoped `-D` + workspace warning-count parity. Clearing 680 downstream warnings is a separate effort, out of "minimal changes" scope. |
| `panic!` vs `assert!(false)` semantic drift | None | Identical panic behavior; messages preserved; only in `#[cfg(test)]`. |
| Match-arm merge changes semantics | Very low | Bodies identical by definition of the lint; guards (`hidden: false`) and the `None` arm kept; `RoomId` is `Copy` so `*room_id` unchanged. 318 lib tests + parser tests re-verify. |
| `derive(Default)` changes `CallMode` behavior | None | Derive output is literally `Self::P2p`; MSRV 1.80 fine. |
| `#[allow]` hides future real truncation/large-err bugs | Low | `Span` offsets are bounded by chat-line input limits (server-side caps); allow is the established in-crate pattern (`metrics.rs:737`, `aero-live-whip/relay.rs`, `aero-ai/embed.rs`). |
| config.rs signature change ripple | Avoided | Allow chosen over `Box<figment::Error>` → zero caller impact (3 verified call sites). |
| Downstream compile breakage | None | All changes internal to aero-common's own code; no public API, schema, event, or config change. |
| CI gates protecting us | — | `cargo check --all-targets`, `cargo test --workspace --lib` + `authz_lint` + `aero-cli gate all` (truth/file-size checks) all run in CI; the per-crate `-D warnings` clippy run is the new stricter local bar. |

**Net change**: 7 files in `crates/aero-common/src`, ~40 lines touched, zero behavior/API/schema/config changes, no new dependencies, fully revertible by reverting one commit.
