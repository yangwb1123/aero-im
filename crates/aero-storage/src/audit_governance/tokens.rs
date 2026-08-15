//! B5-1 auth-slice governance tokens — the storage-side single Rust source
//! (connector design §2.7 / landing design §2.2, D1).
//!
//! Every literal below is the §2.7 allowlist: for each local token the in-tx
//! producer writes exactly 1 `audit_events` row + exactly 1
//! `audit_governance_outbox` row (`event_id = audit_events.id` 1:1, class
//! 'admin', priority 10, outbound token per the table below) in the SAME
//! transaction, via [`AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open`]
//! (or the route-orchestrated `_in_tx` variants).
//!
//! Drift guards: `crates/aero-auth/src/audit_tokens.rs` mirrors this
//! vocabulary textually with its `auth_tokens_are_pinned_verbatim` pin test,
//! and `db_tests/auth.rs` (`auth_outbox_parity_1to1` /
//! `auth_allowlist_no_miss_write`) recomputes every envelope field from these
//! constants — never inline literals (lanes.rs G2-closure pattern).
//!
//! | Operation | Local token | Outbound token |
//! |---|---|---|
//! | register (register_enrolled) | `auth.register` | `admin.auth.register` |
//! | login success (handler, post-2FA) | `auth.login` | `admin.auth.login` |
//! | refresh success | `auth.refresh` | `admin.auth.refresh` |
//! | PAT mint | `auth.pat.issue` | `admin.auth.pat.issue` |
//! | PAT revoke | `auth.pat.revoke` | `admin.auth.pat.revoke` |
//! | TOTP enroll/activate | `auth.totp.enroll` | `admin.auth.totp.enroll` |
//! | session revoke (user side) | `session.revoked` | `admin.auth.session.revoke` |
//! | session revoke (admin side) | `session.revoked.admin` | `admin.auth.session.revoke` |
//! | login failure (L1 window, no audit row) | — | `admin.auth.login.failure` |

/// Account created by first-party registration (`register_enrolled`, riding
/// `RegistrationRepo::create`'s transaction; pair detail `{}` — PII-reduced,
/// see D-N1). SSO/OIDC JIT registrations stay audit-only (`None` `auth_audit`).
pub const AUTH_REGISTER: &str = "auth.register";
/// Login success — emitted at the HTTP handler AFTER the 2FA gate passes
/// (D5: must never move into `AuthService::login` — a 2FA-failed attempt
/// would be recorded as a successful login). Standalone pair, nil workspace.
pub const AUTH_LOGIN: &str = "auth.login";
/// Refresh success (`AuthService::refresh`). Standalone pair, nil workspace.
pub const AUTH_REFRESH: &str = "auth.refresh";
/// PAT minted (route-orchestrated `PatRepo::create_in_tx` + pair).
/// RENAMED from `auth.pat.create` (DP-1 — §2.7 is normative).
pub const AUTH_PAT_ISSUE: &str = "auth.pat.issue";
/// PAT revoked (route-orchestrated `PatRepo::revoke_in_tx` + pair; only when
/// a still-active owned row was revoked — no-op revokes audit nothing).
pub const AUTH_PAT_REVOKE: &str = "auth.pat.revoke";
/// TOTP enrolled/activated (route-orchestrated `TotpRepo` `_in_tx` variants +
/// pair, detail `{"stage":"enroll"|"activate"}`; pair only when a row flipped).
/// REPLACES `auth.totp.enabled` (DP-1).
pub const AUTH_TOTP_ENROLL: &str = "auth.totp.enroll";
/// TOTP removed (`TotpRepo::disable`, in-tx). RESTORED from the DP-1 merge
/// (design-gate F-2): 2FA removal must never be recorded as enrollment.
/// Audit-only (outside the §2.7 pair table; a production no-miss-write
/// monitor must scope to paired rows, same as D-N4's JIT scoping).
pub const AUTH_TOTP_DISABLED: &str = "auth.totp.disabled";
/// Session revoked by its owner (inventory `DELETE /api/auth/sessions/:sid`
/// and logout). Kept for trajectory continuity (D6) — existing consumers of
/// the `session.revoked` audit trail are unaffected.
pub const SESSION_REVOKED: &str = "session.revoked";
/// Session revoked by a workspace owner/admin force-revoke (embedded in the
/// existing `admin_revoke.rs` transaction; route zero-change).
pub const SESSION_REVOKED_ADMIN: &str = "session.revoked.admin";

// --- Outbound tokens (payload.action) ---
pub const OUTBOUND_AUTH_REGISTER: &str = "admin.auth.register";
pub const OUTBOUND_AUTH_LOGIN: &str = "admin.auth.login";
pub const OUTBOUND_AUTH_REFRESH: &str = "admin.auth.refresh";
pub const OUTBOUND_AUTH_PAT_ISSUE: &str = "admin.auth.pat.issue";
pub const OUTBOUND_AUTH_PAT_REVOKE: &str = "admin.auth.pat.revoke";
pub const OUTBOUND_AUTH_TOTP_ENROLL: &str = "admin.auth.totp.enroll";
pub const OUTBOUND_AUTH_SESSION_REVOKE: &str = "admin.auth.session.revoke";
/// L1 login-failure aggregation row (class 'message', priority 10; no
/// `audit_events` row — D7 exemption).
pub const OUTBOUND_AUTH_LOGIN_FAILURE: &str = "admin.auth.login.failure";

/// Envelope `source_system` for auth explicit writes (the 0239 moderation
/// trigger uses `binding.source_system`; auth rows are written explicitly,
/// never by trigger-token extension — D1).
///
/// **C1 (design-gate blocker)**: this MUST equal the leaf
/// [`aero_common::model::audit::AUDIT_SOURCE_SYSTEM`] — the connector's
/// `validate_delivery_payload` (client.rs:518) dead-letters any row whose
/// `source_system != AERO_AUDIT_SOURCE_SYSTEM` (default `aero-im.source`);
/// a local `"aero-auth"` spelling killed the entire auth lane at the relay.
pub const AUTH_SOURCE_SYSTEM: &str = aero_common::model::audit::AUDIT_SOURCE_SYSTEM;
/// Local action token carried by the L1 aggregation envelope's `payload`
/// (there is no `audit_events` row for it — D7).
pub const L1_AUTH_LOGIN_FAILURE_ACTION: &str = "auth.login.failure";
