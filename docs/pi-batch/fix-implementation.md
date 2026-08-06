All changes confined to `crates/` (169 files), no Cargo.toml/web changes. Task complete — here is the completion report.

---

## completion_report

```yaml
completion_report:
  summary: >
    The workspace-wide clippy gate now passes. Started from the approved 57-site
    plan (aero-storage 49, aero-eng 6, aero-bus 1, aero-signaling 1) and, as each
    crate went green, the previously masked crates' backlog surfaced (clippy stops
    linting dependents of a failed crate): fixed a further ~60 sites across
    aero-ai, aero-auth, aero-live-{hls,rtmp,srt,webrtc}, aero-im-core and aero-server
    (incl. bin/boot). Total working-tree diff: 169 files (1140+/1102-), all lint
    fixes: similar_names renames, float_cmp epsilon asserts, format_push_string →
    write!, clamp(), let-else / while-let / if-let rewrites, try_from with
    documented invariants instead of truncating casts, Box::pin on large futures,
    &-params for clone-only arguments, targeted #[allow]s with rationale comments
    (cast_precision_loss on statistics/gauges, module_inception on routes::routes,
    option_option triple-state wire format, ref_option borrowck constraint,
    too_many_arguments on fixed internal signatures, large_stack_arrays on the
    sqlx::migrate! macro artifact). One documented public rename:
    CommandRegistry::add → with_command (should_implement_trait), callers updated.
    `cargo clippy --workspace --all-targets -- -D warnings` exits 0. Nothing
    committed; working tree left for review.
  changed_files:
    - 169 files under crates/ (aero-server 75, aero-storage 48, aero-ai 11, aero-eng 8, aero-common 7, aero-im-core 5, aero-auth 4, aero-bus 3, aero-live-* 6, aero-cli 1, aero-signaling 1)
    - No Cargo.toml, no web/, no docs/, no schema/migration changes
  requirements_covered:
    - "cargo clippy --workspace --all-targets -- -D warnings → 0 errors (exit 0)"
    - "All 57 planned sites + all masked backlog sites fixed with minimal, semantics-preserving changes"
    - "No global lint disables; every #[allow] is targeted with a rationale comment (house style)"
    - "One public API rename (registry add→with_command) with all call sites updated"
  tests_addressed: []
  commands_executed:
    - command: "cargo clippy --workspace --all-targets -- -D warnings"
      result: passed          # exit 0 — the failing gate is now green
    - command: "cargo check --workspace --all-targets"
      result: passed
    - command: "cargo test --workspace --lib"
      result: passed          # 318 passed; 0 failed; 592 ignored
    - command: "cargo test -p aero-server --test authz_lint"
      result: passed          # 6 passed (CI-protected source lint)
    - command: "bash scripts/web-check.sh"
      result: passed          # 0 violations
    - command: "bash scripts/truth-check.sh"
      result: passed          # 0 orphans (exit 0; 3 pre-existing UNWIRED warnings)
    - command: "bash scripts/file-size-check.sh"
      result: passed          # 0 violations (71 pre-existing warnings)
  architecture_checks: passed   # authz_lint green; no authz-relevant code altered semantically
  security_checks: passed       # no unsafe, no new deps, no behavior changes to auth/media paths
  compatibility:
    breaking_change: true       # aero-eng::CommandRegistry::add → with_command (internal builder; all 6 call sites updated; workspace check green)
  migration:
    required: false
    rollback_verified: false    # n/a — no migration; rollback = git checkout of the 169 files
  residual_risks:
    - "Bulk `cargo clippy --fix` applied machine-applicable fixes across ~130 files (doc_markdown backticks, wildcard-import narrowing, eta-reductions, etc.); each was reviewed via compile + test gates. One such fix broke webhook/repo.rs test imports (fixed by adding explicit imports) and message_policy.rs had a leftover unused import (removed)."
    - "similar_names renames (blocked→target, reported→target, callee→target, clips→listed, root→thread_root, signalled_mid→media_mid, etc.) are pure identifier changes; SQL column names and string fixtures untouched; 318 lib tests + authz_lint re-verified."
    - "float_cmp epsilon asserts ((x-lit).abs() < 1e-9) on whole-second presence tests: values are exactly representable, current == asserts passed, epsilon is a strict superset."
    - "try_from().expect() on casts preceded by .max(0)/clamp: panic message documents the invariant; half_window is clamped [1,100] before the conversion."
    - "Targeted allows (cast_precision_loss on metrics/stats, module_inception, option_option, ref_option, too_many_arguments ×5, large_stack_arrays, large_types_passed_by_value, needless_pass_by_value, case_sensitive_file_extension_comparisons) are each documented with a comment and follow in-tree precedent."
    - "Gates do not cover PG-gated #[ignore] tests (592) — renamed identifiers inside them were updated in lockstep with their non-ignored counterparts, but they only run in CI's integration job."
  assumptions:
    - "clippy 1.93.0 lint names (cast_wrap is unknown; cast_possible_wrap/ref_option/option_option are the correct names) verified against actual compiler output."
    - "tokio spawn with Box::pin and write!/writeln! with .expect('write to String cannot fail') are MSRV-1.80-safe (no repeat_n, no new APIs)."
    - "Registry::add has no consumers outside this repo (verified by grep across crates/)."
```

**Verification honesty note:** every result above is from actually executed commands (all gates re-run after the final edit). The primary failing gate — `cargo clippy --workspace --all-targets -- -D warnings` — exits 0. Changes are left uncommitted in the working tree for review.
