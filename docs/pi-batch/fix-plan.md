All reproduction and evidence gathering complete. Here is the fix plan.

---

# Fix Plan: aero-common clippy gate (`-D warnings`)

## ⚠️ State finding (must-read before the plan)

The task premise — *"clippy currently errors on crates/aero-common: 16 errors in lib + 29 in lib-test"* — describes the **pre-fix state**. The working tree already contains the complete fix from the prior implement stage (uncommitted, per batch rules: `git status` shows exactly the 7 planned files modified; the `[aero-im-batch] Stage: plan/implement` commits contain only docs artifacts `docs/pi-batch/fix-plan.md` / `fix-implementation.md`). Reproduction against the **actual current state**:

| Gate | Current result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | ❌ fails — but **0 hits in aero-common**; fails on the pre-existing downstream backlog (aero-storage 203 sites, aero-eng 29, aero-bus 6, aero-signaling 1; compilation halts at aero-storage) |
| `cargo clippy -p aero-common --all-targets -- -D warnings` | ✅ PASS (0 errors — the crate-scoped gate is green) |
| `cargo test --workspace --lib` | ✅ PASS (318 passed, 0 failed, 592 ignored) |
| `bash scripts/web-check.sh` | ✅ PASS (0 violations) |

**Conclusion: no further code changes are needed.** The remaining work is verification + commit of the already-applied 7-file diff. The plan below documents root causes, the applied change radius, and the honest boundary of the workspace gate.

## 1. Root causes (evidence)

The failure was 8 clippy lint classes × **29 unique sites**, all in `crates/aero-common/src` (16 lib + 29 lib-test = 29 unique; the 13 extra lib-test errors are the `#[cfg(test)]`-only hits). All are pre-existing style backlog, no single regression, no behavior bug:

| Class | Count | Sites (clippy 1.93.0 output, anchored in applied diff) |
|---|---|---|
| `assertions_on_constants` | 13 | `markdown.rs` `#[cfg(test)] mod tests`: `bold`, `italic`, `code`, `strikethrough`, `link`, `multiple_formats_in_one_line`, `plain_text_passes_through`, `unclosed_delimiter_is_literal`, `asterisk_in_word_not_italic`, `multi_line`, `link_no_href_is_literal`, `quadruple_asterisk_is_literal` — old test-failure idiom `assert!(false, "…")` |
| `doc_markdown` | 8 | `markdown.rs` module docs (`CommonMark`, `snake_case`, `ParticipantId`); `mls.rs` (`KeyPackage`); `model/mod.rs` (`REFACTOR_PLAN.md`); `model/event.rs` `NotifyBatch` docs (`NotifyBatch`, `ULID`, `delivery_id`, `participant_id`) — identifiers written without backticks |
| `match_same_arms` | 2 | `model/block.rs` `Block::searchable_text` (`Text\|Code` vs `Thought{hidden:false}` both `Some(content)`); `model/event.rs` `RoomEvent::room_id` (12 flat room_id variants vs `Call(Invite\|End\|Caption\|Join\|Leave\|SfuPublisher)` both `Some(*room_id)`) |
| `cast_possible_truncation` | 2 | `markdown.rs` Phase-2 `RawSpan`→`Span` conversion (`r.start as u32`, `r.end as u32`; `Span.start/end` are u32 by wire-format design) |
| `derivable_impls` | 1 | `model/media.rs` manual `impl Default for CallMode` returning `Self::P2p` |
| `result_large_err` | 1 | `config.rs` `AppConfig::load() -> Result<Self, figment::Error>` (`figment::Error` ≥ 208 bytes) |
| `redundant_closure` | 1 | `markdown.rs` `parse_markdown_to_blocks`: `.flat_map(|line| parse_line(line))` |
| `items_after_statements` | 1 | `markdown.rs` `parse_spans`: `struct RawSpan` declared after `let bytes`/`let len` |

CI context (`.github/workflows/ci.yml`, job `check`): *"The workspace intentionally carries a documented clippy backlog; the repository gates prohibit new warnings rather than pretending the existing backlog can be promoted to hard errors."* — CI runs clippy **without** `-D warnings`.

## 2. Module boundary & change radius (per `backend-specs/agent-guardrails.md` §2)

- **直接修改文件**: 7 files, all under `crates/aero-common/src/` — `markdown.rs`, `config.rs`, `mls.rs`, `model/mod.rs`, `model/block.rs`, `model/event.rs`, `model/media.rs` (diff: 40 insertions, 38 deletions — verified in tree)
- **间接影响模块**: none — aero-common is the leaf crate; zero runtime behavior change
- **公共接口变化**: **none** — `AppConfig::load` signature kept (`Result<Self, figment::Error>`); `CallMode` derives identical `Default`; arm merges are identical bodies; `panic!` ≡ `assert!(false,…)` semantics
- **数据库/事件/配置/部署变化**: none. **回滚方式**: `git checkout --` the 7 files (or revert the eventual commit)
- **Not touched**: root `Cargo.toml` lints, `web/`, any other crate, no crate-level `#[allow]` (only targeted allows, house style — in-crate precedent `metrics.rs`)

## 3. Exact files/symbols (as applied in the working tree — verified by `git diff`)

1. **`markdown.rs`** — module doc backticks (3); `.flat_map(parse_line)`; `struct RawSpan` hoisted above statements in `parse_spans`; `#[allow(clippy::cast_possible_truncation)]` on the Phase-2 `let spans` statement with rationale comment; 13× `panic!("…")` with messages preserved verbatim.
2. **`config.rs`** — `#[allow(clippy::result_large_err)]` + comment on `AppConfig::load` (keeps public signature; 3 callers in aero-server bins untouched).
3. **`mls.rs`** — `` `KeyPackage` `` in doc comment.
4. **`model/mod.rs`** — `` `REFACTOR_PLAN.md` `` in module doc.
5. **`model/block.rs`** — `Block::searchable_text`: merge `Thought{hidden:false}` into the `Text|Code` arm (guard preserved).
6. **`model/event.rs`** — `NotifyBatch` field docs backticks; `RoomEvent::room_id`: merge `Call(Invite|End|Caption|Join|Leave|SfuPublisher)` into the flat `Some(*room_id)` arm (`Call(Answer|Ice|Roster|Offer) => None` untouched).
7. **`model/media.rs`** — `CallMode`: `Default` added to derive + `#[default]` on `P2p`, manual impl removed.

## 4. Test plan

Already executed (all pass): `cargo clippy -p aero-common --all-targets -- -D warnings` (0 errors) · `cargo check --workspace --all-targets` · `cargo test --workspace --lib` (318 passed) · `cargo test -p aero-server --test authz_lint` (6 passed, from implement stage) · `scripts/web-check.sh` (0 violations) · `scripts/truth-check.sh` (0 orphans) · `scripts/file-size-check.sh` (0 violations) · workspace clippy non-`-D`: aero-common contributes **0 warnings**, workspace total 680→649 (delta = exactly the removed aero-common lines; **no new warnings anywhere**).

**Remaining action**: commit the 7-file diff (currently uncommitted in the working tree for review).

## 5. Risk assessment

| Risk | Likelihood | Mitigation / gates |
|---|---|---|
| Workspace-wide `-D warnings` still red after this fix | Certain | Fails on the documented pre-existing backlog (aero-storage 203, aero-eng 29, aero-bus 6, aero-signaling 1, plus aero-server/aero-cli/aero-live-* masked behind aero-storage's halt). CI deliberately runs clippy without `-D`; AGENTS.md §4.3 requires "别新增警告" (no new warnings), not zero backlog. Must be stated honestly in the commit/PR: green bar = crate-scoped `-D` + workspace warning parity. Clearing ~649 downstream warnings is a separate effort. |
| `panic!` vs `assert!(false)` drift | None | Identical panic semantics; messages preserved; test-only. |
| Arm-merge semantic change | Very low | Bodies identical by lint definition; guards (`hidden: false`) and `None` arm kept; `RoomId` is `Copy`. 318 lib tests re-verify. |
| `derive(Default)` change | None | Literally `Self::P2p`; MSRV 1.80 ≫ 1.62. |
| Targeted `#[allow]`s hide future bugs | Low | `Span` offsets bounded by chat-line caps; config load once at boot; comments in code; in-crate precedent (`metrics.rs`). |
| Working-tree fix silently lost / double-applied | Low | Batch flow: verify the 7-file diff is present before commit; do not re-apply edits (would fail on oldText mismatch). |
| CI gates protecting us | — | `cargo check --all-targets`, `cargo test --workspace --lib` + `authz_lint` + `aero-cli gate all` (truth/file-size) all green; per-crate `-D` clippy is the stricter local bar. |

**Bottom line**: the aero-common clippy backlog is already fully fixed in the working tree (29/29 sites, 7 files, zero behavior change). No further code modification is required — the plan's remaining step is commit + the honest boundary note that workspace-wide `-D warnings` will continue to fail on the documented downstream backlog.
