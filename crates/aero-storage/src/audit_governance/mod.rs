//! `audit_governance_outbox` (v2) contract fixtures — PG `db_tests` for the
//! 0239 migration landed by the B5-1 migration slice.
//!
//! Harness contract (`scripts/test-integration.sh` `run_migrated_integration`
//! entries, gated on `migrations/0239_audit_governance_outbox.sql`):
//!   * filter `audit_governance::` — this module's `db_tests` (the harness
//!     empty-filter guard requires ≥1 matched test; a vacuous green is a
//!     FAIL, never a pass);
//!   * filter `moderation_finalize_outbox_parity` — the named parity test
//!     below, the **in-tx oracle** for the additive 0239 trigger (F9: the
//!     relay drills seed the table directly and cannot catch a trigger that
//!     never fires).
//!
//! Cross-slice pins: every literal asserted here mirrors
//! `aero_common::model::audit` (the leaf single source: `GOVERNANCE_CLASS_ADMIN`,
//! `MODERATION_OUTBOUND_ACTION`, `LOCAL_ACTION_MODERATED`),
//! `aero_ai::governance::GOVERNANCE_PRIORITY_MODERATION = 100`,
//! `GOVERNANCE_PRIORITY_BACKLOG = 10`, and the 0239 SQL literals verbatim.
//! aero-storage must not depend on aero-ai (dependency direction), so the
//! leaf constants + these behavioral tests are the drift guards.
//!
//! The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a
//! sibling slice (handoff H3 of the 0239 design doc); this module carries the
//! DDL-contract fixtures only — add the repo to this same module when it
//! lands, keeping the parity test's name and literals stable (it is now a
//! pinned contract fixture).
//!
//! Module layout (B5-1 producer seam): `db_tests/` holds the fixture helpers
//! (this module's parent level) plus the test bodies split by trigger/domain
//! — `parity` / `ddl` / `gates` / `dedup` / `l1` / `lanes` — and the
//! `producer` submodule with the in-tx audit-append seam tests (S1–S4).
//! Every submodule opens `use super::*;`; helpers shared across domains live
//! in `db_tests.rs` so the submodule test bodies stay byte-identical to the
//! pre-split file.
//!
//! ## H3 landed (auth slice)
//!
//! [`outbox`] = the v2 enqueue-side repo (`AuditGovernanceOutboxRepo`:
//! envelope mirror, SAVEPOINT fail-open pair, standalone pair, L1
//! aggregation, error classifier); [`failed_pairs`] = the 0244 DLQ
//! compensation repo; [`tokens`] = the §2.7 allowlist (storage is the single
//! Rust source; `aero-auth::audit_tokens` mirrors textually with its pin
//! test). IMMEDIATE-constraint pin (D4): 0239/0236 CHECK/RAISE are all
//! IMMEDIATE — a future DEFERRABLE change defers errors to COMMIT and breaks
//! the SAVEPOINT fail-open branch (whole-tx fail-closed, violates R7); see
//! also migration 0244's DDL comment.

pub mod failed_pairs;
pub mod outbox;
pub mod tokens;

pub use failed_pairs::FailedPairRepo;
pub use outbox::{AuditGovernanceOutboxRepo, governance_envelope, is_fail_open_error};
pub use tokens::{
    AUTH_LOGIN, AUTH_PAT_ISSUE, AUTH_PAT_REVOKE, AUTH_REFRESH, AUTH_REGISTER, AUTH_SOURCE_SYSTEM,
    AUTH_TOTP_ENROLL, L1_AUTH_LOGIN_FAILURE_ACTION, OUTBOUND_AUTH_LOGIN, OUTBOUND_AUTH_LOGIN_FAILURE,
    OUTBOUND_AUTH_PAT_ISSUE, OUTBOUND_AUTH_PAT_REVOKE, OUTBOUND_AUTH_REGISTER, OUTBOUND_AUTH_REFRESH,
    OUTBOUND_AUTH_SESSION_REVOKE, OUTBOUND_AUTH_TOTP_ENROLL, SESSION_REVOKED, SESSION_REVOKED_ADMIN,
};

#[cfg(test)]
mod db_tests;
