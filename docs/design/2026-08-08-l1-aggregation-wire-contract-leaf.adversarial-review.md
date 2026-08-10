# Adversarial review — `2026-08-08-l1-aggregation-wire-contract-leaf.design.md`

Scope: audit-trail integrity of the 19-field window row · dual-form idempotency disjointness proof · C1 silent-swallow · rule 3e guard bypass surface · F1–F10 adversarial coverage · leaf-scope mitigations.

**Verdict: the wire vocabulary itself is sound; three real findings (F11 C1-swallow is live in this design, not future; F15 window delivery can never pass the in-repo receipt contract; rule 3e comment-strip semantics are mis-claimed vs rule 3d), four secondary gaps, plus the two already-flagged corrections (C2 `--test-threads=1`, harness orphan-confound) re-verified.**

---

## 1. Dual-form idempotency — disjointness PROVEN, two traps the design misses

### 1.1 Proof: no string is valid in both spellings

Two parsers, both read from source:

- **ULID base32** (`ulid-1.2.1/src/base32.rs`): `decode()` first checks `encoded.len() != ULID_LEN (26) → Err(InvalidLength)`; charset = Crockford `0123456789ABCDEFGHJKMNPQRSTVWXYZ` (+ lowercase via LOOKUP), `-` → `InvalidChar`. `AuditId::from_str` / `AuditWindowId::from_str` delegate here (ids.rs `define_id!` FromStr → `Ulid::from_str` → `from_string` → `base32::decode`).
- **`uuid::Uuid::parse_str`** accepts only simple (32 hex), hyphenated (36, dashes at 8/13/18/23), braced (38), or `urn:` (45) shapes.

Length disjointness alone settles it: ULID-valid ⇒ |s| = 26; UUID-valid ⇒ |s| ∈ {32, 36, 38, 45}. **No string can satisfy both.** Charset is a second, independent barrier (`-` rejected by ULID; Crockford-only letters rejected by hex). Therefore `receipt_event_id_matches`'s UUID-first-else-base32 order (client.rs:531-532) is deterministic: at most one branch succeeds for any echo; both-fail → `false` → `Permanent(ReceiptMismatch)` — fail-closed. Same proof holds for window keys (`AuditWindowId` is the same macro surface). The base32 form is additionally lexicographically order-preserving (fixed 26 chars) — no hidden ordering trap in either spelling.

### 1.2 Trap F15 (NEW, real): a window row can never be delivered under today's receipt contract

The design's AC4 test has `claim_due` **claim window rows through the 1:1 path** (`window_backlog_never_preempts_moderation`: claimed set = {admin} ∪ {earliest backlog∪window}; the design's own R8 pins this). `ClaimRow → Claim` downcasts `AuditId::from_uuid(row.event_id)` (pg.rs:68) — so claim/settle/requeue/mark_dead all work on window rows today.

But the **delivery + receipt leg breaks**: the in-repo stub sink echoes `payload.get("event_id")` (stub.rs:307-314) — the 1:1 contract's echo source. The 19-field window envelope has **no `event_id` key** (window_id/count/first_event_id/last_event_id replace it) → extraction yields `None` → echo = `"missing"` → `receipt_event_id_matches("missing", …)` = false → `ReceiptMismatch` → permanent → **window row dead after ≤1 retry, never delivered**. `validate_delivery_payload` passes (source_system present, no tenant_id), so the row is POSTed and only fails at receipt — one wasted POST, then permanent loss.

AC4/AC5 never exercise the delivery leg (AC5 is claim→requeue→reclaim, client-side only), so this cannot be caught by the planned tests. The design's §7 carve-out ("connector claim 键控 out of scope") is contradicted by its own AC4 pinning the claim path; the *receipt* contract for window rows is a real wire dependency that must be named (sink must echo header or `idempotency_key`, never a member id) and a stub-mode delivery test added in the connector slice.

### 1.3 Gap: no test asserts the two spellings denote the same id

AC2 covers base32 roundtrip; the exact-wire test asserts `idempotency_key == window_id` *within the envelope* (both uuid text). Nothing asserts **header base32 ↔ envelope uuid text value-equality** — the property `receipt_event_id_matches` relies on. Add a 3-line leaf test (mitigation M3).

---

## 2. C1 — window-id-derived-from-audit-id: CONFIRMED live, and the design's F2 mitigation is incomplete

SQL verified: 0239 trigger INSERT is `ON CONFLICT (event_id) DO NOTHING`; 0241 uses `NOT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE outbox.event_id = audit.id)`. A window row whose `event_id` equals a real `audit_events.id` **silently suppresses both** the trigger enqueue and the reconciler backfill. Severity beyond the DB reviewer's demonstration:

- **Double-silent**: 0241 is the parity mechanism (disabled-window self-heal). The swallowed event is invisible to it too — no self-heal, no alert, `COUNT(outbox) != COUNT(audit)` divergence with no detector (the A2 drills use their own fixtures; nothing continuously cross-checks parity in production).
- **F2's mitigation is incomplete**: "type-disjoint" holds only for *direct* `AuditId`→`AuditWindowId` passing. `AuditWindowId::from_uuid(an_audit_id.to_uuid())` compiles (both are transparent Ulid wrappers — pg.rs:68 already performs the mirror downcast), and the B5-1 SQL producer is typeless entirely. There is no compile-time or DDL layer that can express "window id ∉ audit_events.id".
- **Worse than future**: the design's own AC4 test pins window rows as claimable by the 1:1 path *today* — the "keying decision" the design defers is de facto settled (claim_due + AuditId downcast + value-level receipt). The only un-settled piece is producer discipline = the leaf constructor, which is exactly where the design *can* mitigate cheaply (M1). A mis-keyed window row would additionally be *delivered* as if it were the 1:1 moderation event (sink ledger records a delivered row for event X whose payload is a batch envelope) — audit-trail integrity broken at the sink.

0241's NOT EXISTS is also the reason a `CHECK` can never fix this: the constraint would need anti-joins to a retention-swept table. Residual must be stated honestly (M1 + F11).

---

## 3. Audit-trail integrity of the 19-field window row

**Pinned (verified):** payload is byte-immutable across all four connector ops (none of the `SET` clauses touch `payload` — claim sets status/token/lease/attempts; requeue/mark_dead set status/available_at/token/lease/last_error); `idempotency_key == window_id` constructor-enforced; span ordering correct under canonical spelling; `Dead` terminal; no outbox-row sweeps exist (all DELETE/TRUNCATE sites are test-only — window rows persist).

**Residuals the design should name, not assume covered:**

- **Content truthfulness**: `payload` is opaque jsonb (0239 CHECK = object-ness only); `count`/`first_event_id`/`last_event_id` can lie and no leaf or DDL layer sees it. AC4 proves *representability*, not integrity. This is B5-1 A4's job — say so explicitly in §4 (F14).
- **Canonical-form precondition of `span_is_ordered`**: `first_event_id`/`last_event_id` are `String` (wire fidelity); the lexicographic compare is only correct for canonical lowercase hyphenated uuid text. Constructor-enforced today; F9 struct-literal bypass breaks it **silently** (garbage strings → silently wrong ordering answer). Harden: parse-based compare (M2) — fail-closed instead of silently wrong.
- **Dead window rows have no recovery path**: 0241 is moderation-only, so a dead window row is permanently undelivered. The member `audit_events` rows survive (trail intact) — delivery loss only, same manual posture as 1:1 dead rows. One doc line.

---

## 4. Rule 3e guard — four findings

### 4.1 Comment-strip semantics mis-claimed (design §2.4 vs the mechanism it mirrors)

Rule 3d — the mechanism 3e claims to mirror — strips **neither** comments **nor** tests components (its loop checks only `CLAIM_AUDIT_FILE` + `AUDIT_FLAG_ALLOWED`; the `is_comment_line`/`skip_tests_component` calls at truth-check-lib.sh:230-231/:268-269 belong to 3a/3c). The design's "skip：tests 组件 / 注释引导行（继承 skip_tests_component / 注释剥离）" is therefore a 3a-style choice mislabeled as 3d-mirroring. Consequences:

- If 3e strips comments (3a style): a `// "message.batch"` in any non-exempt file is clean — fine (comments don't execute) but must be pinned by a harness case; and `/* */` block comments are NOT stripped by `^\s*//` — those still flag (fail-closed, good).
- If 3e mirrors 3d exactly (recommended): most fail-closed; the leaf's own `//!`/`///` doc mentions of the token are already exempt via `CLAIM_AUDIT_FILE`; zero noise today (`rg '"message.batch"' crates` = 0 hits, verified).

Pick one, state it, add the harness case. The current design text is ambiguous between the two.

### 4.2 Fail-open vectors in the scan (all inherited from 3a–3d, all should be named for 3e)

1. **rg error masking**: every scan is `rg … 2>/dev/null || true` — an rg exit ≥ 2 (config error, unreadable dir) is treated as *zero hits* → scan looks clean. The "rg not found" branch is fail-closed, but errors are fail-open. Cheap fix: capture exit code; ≥2 → violation (M5).
2. **Invisible files**: rg by default skips hidden (`.secret.rs`), gitignored, and symlinked files during `crates` traversal. A literal in any of those is invisible to 3a–3d and would be to 3e. `--hidden`/`--follow` are optional hardening; at minimum state the boundary.
3. **`-F` exact-match**: `concat!`/`format!`/`"message." "batch"` spellings bypass. Accepted limitation of every literal tripwire (same as 3d) — the guard pins the canonical spelling + single-source, not adversarial obfuscation. State it so nobody later "fixes" it with false confidence.
4. **Leaf-file wholesale exemption**: any second `"message.batch"` spelling *inside audit.rs* (a stray helper, a duplicate const) is invisible — path exemption can't distinguish definition from duplication. Optional count arm (M6).

### 4.3 Allowlist key-format collisions with rule 3d — none functional; one shared pin

Keys are `<path>:<line>` in both rules with **separate arrays and per-literal stale greps** (`grep -qiF '"admin.content.flag"'` vs `'"message.batch"'`) — a 3e entry can only exempt 3e's literal; no cross-rule masking. The one shared, fragile point is **`CLAIM_AUDIT_FILE` itself**: it is the exemption for *both* rules. A restructure of `audit.rs` → `audit/mod.rs` breaks both pins simultaneously (rule 3d's doc comments in audit.rs would suddenly flag too). This kills the design's D1 "tests 子模块" fallback leg: inline `#[cfg(test)] mod tests` gives zero size relief; the directory form breaks `CLAIM_AUDIT_FILE`. **Only `crates/aero-common/tests/` is viable** (`skip_tests_component` exempts any `tests` path component; truth-check §1 orphan scan only walks `crates/*/src`), with the command ripple: `cargo test -p aero-common --lib` does not run integration tests → `cargo test -p aero-common` (§6 AC1/AC2 commands must change).

### 4.4 Harness "临时 .rs 于 crates/ 下" — orphan-confound confirmed

Re-verified: truth-check.sh:232 `exit $((orphan_violations + CLAIM_GUARD_VIOLATIONS))`; the orphan scan walks every `crates/*/src` .rs not `mod`-declared; the harness fold case asserts the exact string `结果: 0 个 ORPHAN, 3 个 UNWIRED, 1 个 CLAIM-GUARD` + exit 1. A temp `.rs` under `crates/*/src` adds ORPHAN=1 → exit 2 → fold fails. Note the **spec's own AC3 wording** ("a temp `.rs` under `crates/`…") is the source of the design's copy — both must be reworded to the established append-to-existing-file pattern (n1's jwt.rs precedent) or an orphan-exempt path. Additionally: the stale-warning case is vacuous while `AUDIT_WINDOW_ALLOWLIST` is empty — specify the seeding strategy (sed-inject a pin into the fixture's leaf copy, break the site, expect warning), and add the comment-line injection case (4.1).

---

## 5. F1–F10 coverage — mechanical-only; five adversarial gaps

F1–F10 are all *drift alarms* (correct and well-targeted). They do not cover adversarial/negligent-producer failures:

| # | Missing adversarial failure | Status |
|---|---|---|
| **F11** | **C1**: window id derived from a real audit id → 0239 enqueue + 0241 backfill both silently swallowed; the row is then *delivered as* the 1:1 event (sink ledger corruption). | **Live hazard in this design** (AC4 pins the 1:1 claim path). F2's "type-disjoint" mitigation is incomplete (downcast compiles; SQL typeless). Must become a named failure mode with M1 + residual statement. |
| **F12** | Window row mislabeled class 'admin' / priority 100 by a buggy producer → indistinguishable from moderation; breaks `is_admin_class`'s 1:1 guarantee at the SQL layer (Rust-side contract only). F3 only covers correctly-labeled priority-10 rows. | Residual; document. AC1's `aggregated_action_token_is_pinned` pins the vocabulary, not the row. |
| **F13** | Window-id reuse across windows (producer reuses a stale id) → sink dedup swallows the later window's delivery (per-key at-least-once broken). Leaf cannot detect (no global state). | Residual; document. AC5 pins same-window stability only. |
| **F14** | `count`/span truthfulness (count ≠ N, first/last not actual members, members missing from `audit_events`). AC4 proves representability only. | B5-1 A4's job — the design should say so in §4 rather than leave it implicit. |
| **F15** | Window delivery can never pass the in-repo receipt contract (stub echoes missing `event_id` → `"missing"` → permanent dead). | **NEW finding** (§1.2). Must be named + sink-contract dependency listed in §7. |

Also fold in the two already-flagged required fixes: C2 — both §5/§6 PG commands must add `--test-threads=1` (shared-table TRUNCATE races proven); AC5 wording — sleep must be ≥ `max(backoff, lease)` (backoff(1)=1s equals the lease today; attempts>1 seeds would flake).

---

## 6. Concrete leaf-scope mitigations (costed)

| # | Mitigation | Cost | Effect |
|---|---|---|---|
| **M1** | Constructor `debug_assert_ne!(window_id.to_uuid(), first_event_id.to_uuid())` (+ last) + doc pin "window id never derived from any member or any audit id" | 3 lines | Catches the dominant C1 shape (id derived from a member) at the earliest producer touchpoint; `debug_assert` so release cost is nil. Residual (non-member audit-id collision) stated in F11. |
| **M2** | `span_is_ordered()` parses both fields (`Uuid::parse_str` → `AuditId::from_uuid` → `AuditId` Ord); parse failure → `false` | ~6 lines + adjust AC1 (ordered/reversed assertions unchanged) | Fail-closed on F9 struct-literal garbage instead of silently-wrong ordering; still correct for canonical text (parse-compare == string-compare on canonical spelling, proven in §1.1 protocol chain). |
| **M3** | New leaf test `window_id_spellings_denote_same_id`: `AuditWindowId::from_str(&header) == AuditWindowId::from_uuid(Uuid::parse_str(&envelope_key))` + "neither spelling parses as the other format" | ~10 lines | Locks the two-spelling identity + disjointness in the test suite (closes §1.3 gap). |
| **M4** | Rule 3e: mirror 3d exactly (path + allowlist only; **no** comment-strip / tests-skip) and say so in §2.4; add harness cases: bare literal → 1 violation; `// "message.batch"` comment-led line → violation (3d semantics, fail-closed); stale-seed via sed-inject | ~15 lines harness | Removes the semantic ambiguity (4.1); matches the "mirror of 3d" claim; leaf doc comments stay exempt via CLAIM_AUDIT_FILE. |
| **M5** | `claim_guard_scan`: capture rg exit code per scan; exit ≥ 2 → violation (fail-closed on scan errors) | ~6 lines | Closes the biggest fail-open vector (4.2.1). Optionally add `--hidden`. |
| **M6** (optional) | Count arm: `rg -c '"message.batch"' "$CLAIM_AUDIT_FILE"` ≤ N (definition + doc + pin test) | ~5 lines | Catches duplicate spellings inside the exempt leaf file (4.2.4). |
| **M7** | §6/AC5: name F15 + "sink must echo header or `idempotency_key` for window rows" as a B5-1 dependency in §7; connector slice adds a stub-mode delivery test | design text | Makes the delivery-leg break visible instead of surfacing as dead rows in production. |
| **M8** | Fix the four already-identified text items: §2.4 harness wording (append-to-existing-file, 4.4), §5/§6 `--test-threads=1` (C2), AC5 sleep wording ≥ max(backoff, lease), D1 fallback = `crates/aero-common/tests/` only + `cargo test -p aero-common` (drop `--lib`) | text | Removes the three known flake/confound traps and the broken fallback leg. |

**Required (M1–M5, M7–M8); optional (M6).** None touch DDL, connector SQL, or the 37/37 b5-pin count; all stay inside the leaf + guard-script surface the design already claims.
