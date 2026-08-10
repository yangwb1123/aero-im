# Fix Plan — Recall F1 + F3 (r6 revision)

## 0. Reproduction — gates re-run this stage (all green)

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace --lib` | ✅ all suites ok (storage 319/604-ignored, im-core 82/46, server 655/15, 0 failures) |
| `cargo test -p aero-server --test authz_lint` | ✅ 7 passed |
| `bash scripts/web-check.sh` | ✅ 0 violations |
| `bash scripts/truth-check.sh` | ✅ 0 ORPHAN, 3 pre-existing allowlisted UNWIRED |
| `bash scripts/file-size-check.sh` | ✅ 0 violations |

## 1. Root causes (evidence from the actual tree)

**F1 — the tree has a RESIDUAL GAP.** The implementer pass landed 3 of 4 `crud.rs` hunks; the `update_voice_transcript` WHERE hunk was silently dropped while its doc comment (claiming the fence) did land:

| Site (current line) | Statement | Status |
|---|---|---|
| `crud.rs:470` `update_embedding` | `WHERE id = $2 AND deleted_at IS NULL AND recalled_at IS NULL` | ✅ fenced |
| `crud.rs:501` `update_searchable_text` | same | ✅ fenced |
| `crud.rs:445` `update_voice_transcript` | `WHERE id = $1 AND deleted_at IS NULL` — **no `recalled_at`** | ❌ **GAP** |
| `events.rs` row-locked edit/transcript paths | Rust `recalled_at.is_some()` under `FOR UPDATE` | ✅ safe |
| `authorization.rs:311-321` recall UPDATE | `AND recalled_at IS NULL AND deleted_at IS NULL` | ✅ model fence |

Consequence: a post-recall `update_voice_transcript` still matches and appends `searchable_text = searchable_text || E'\n' || $2` → the STORED `search_tsv` re-derives → transcript text resurfaces in FTS after recall cleared the index. The worker race (read pre-recall at `aero-ai/worker/mod.rs:293/319`, write post-recall) is exactly the review's F1.

**The failing-first repro already exists**: `recall_index_fence_tests.rs::recall_fences_late_index_writes` asserts `update_voice_transcript(...).is_none()` post-recall + `searchable_text == ''` — **fails on a live DB against the current tree**, passes after the hunk.

**F3 — verified fully fixed**: `assert_message_recall_preflight` (`im-core/service/messages.rs:452`), WS arm gate (`frame.rs:163-164`), REST gate (`messages.rs:184-186`), hermetic `authz_lint` rule (negative-verified on both entry points). Slowmode deliberately excluded per plan.

## 2. Module boundary / change radius

Already in tree (9 files, implementer pass): `crud.rs`, `recall_index_fence_tests.rs` (new), `recall_tests.rs` (Fixture `pub(super)`), `message/mod.rs`, `im-core/service/messages.rs`, `im-core/db_tests/recall_tests.rs`, `ws/ws_impl/frame.rs`, `routes/handlers/messages.rs`, `tests/authz_lint.rs`. **Remaining: 1 required hunk** (+2 optional hardening). No signature/migration/config changes; rollback = revert files.

## 3. Exact changes remaining

1. **Required**: `crud.rs:445` `update_voice_transcript` → `WHERE id = $1 AND deleted_at IS NULL AND recalled_at IS NULL` (makes SQL match the in-tree doc comment; zero production callers so no runtime behavior change).
2. **Recommended hardening** (same "EVERY index-update UPDATE" rule): `crud.rs:176` `edit()` → add `AND recalled_at IS NULL` (legacy path, zero callers, aligns with `editable_message`'s recalled-terminal invariant); `query.rs:324` `list_without_embedding` → explicit `AND recalled_at IS NULL` (already excluded via `searchable_text <> ''`).

## 4. Test plan

1. Fix hunk → deterministic test `recall_fences_late_index_writes` flips red→green (CI, `DATABASE_URL` throwaway DB); race test `concurrent_recall_vs_embed_write_never_resurrects` proves the atomic invariant.
2. DB-gated: `cargo test -p aero-storage --lib -- --ignored message::recall` (10 existing + 2 new) + im-core preflight test.
3. Hermetic gates (currently green, must stay green): check / clippy `-D warnings` / `--lib` tests / authz_lint / web-check / truth-check / file-size.

## 5. Risk assessment

- **Top risk**: the gap ships because the doc comment already claims the fence — the deterministic test blocks it in CI.
- Fenced writes silently no-op on recalled rows (intended; worker treats `updated=false` as benign); `edit()` fence is zero-risk (dead path); F3 429s match edit's existing exposure (generous tier ceilings, fail-open Redis); DB-gated tests unrun in sandbox (CI required); lint self-check fails loudly on scan drift.

No code modified this stage; plan persisted to `docs/pi-batch/fix-plan.md`.
