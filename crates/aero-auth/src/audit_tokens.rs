//! Auth security-event audit action tokens.
//!
//! The auth slice emits `audit_events` rows through two producers:
//! * **in-transaction** `AuditRepo::append_in_tx` for same-fate mutations
//!   (register, PAT, TOTP, password, recovery codes, email change, SSO JIT);
//! * **best-effort out-of-tx** `AuditRepo::append` for the login path
//!   (failed login, lockout, failed 2FA — never fails the login).
//!
//! The emitter methods live in `aero-storage` (dependency direction: storage
//! cannot import auth), so the storage-side producers and the
//! `aero-audit-connector` drill seeds write the **literal strings**; the pin
//! test below plus the storage db tests and the governance parity fixture
//! assert those literals verbatim — a rename here MUST be mirrored there or
//! tests fail loudly (textual-pin discipline, mirroring
//! `GOVERNANCE_PRIORITY_*` / `governance_lane_for`).
//!
//! B5-1 (DP-1): the §2.7 pair tokens (`auth.pat.issue`, `auth.totp.enroll`,
//! `auth.login`, `auth.refresh`, `session.revoked`, `session.revoked.admin`)
//! mirror `aero_storage::audit_governance::tokens` (the storage side is the
//! single Rust source for the pair table; this file is its textual mirror,
//! drift-guarded by `auth_tokens_are_pinned_verbatim` + the AC-2/AC-3
//! `db_tests`). `auth.pat.create` → `auth.pat.issue` and
//! `auth.totp.enabled`/`auth.totp.disabled` → `auth.totp.enroll` are the
//! rename half of DP-1; `session.revoked` moved INTO this vocabulary (D6
//! continuity), `session.revoked.admin` is new.
//!
//! Workspace convention: account-level events land in
//! [`WorkspaceId::nil()`](aero_common::WorkspaceId::nil) (the 0006-seeded
//! "legacy / default" tenant — the established account-level security event
//! home, see the `auth.login.new_ip` handler comment); `auth.register` uses
//! the registration's actual `workspace_id`.

/// Account created by first-party registration (`RegistrationRepo::create`,
/// in-tx; also reused by SSO/OIDC JIT provisioning with
/// `detail.provisioning = "sso_jit"`).
pub const AUTH_REGISTER: &str = "auth.register";
/// Login success — emitted at the HTTP handler AFTER the 2FA gate passes
/// (D5: must never move into `AuthService::login`). §2.7 pair token.
pub const AUTH_LOGIN: &str = "auth.login";
/// Failed password / unknown email (`AuthService::login`, best-effort).
pub const AUTH_LOGIN_FAILED: &str = "auth.login.failed";
/// Lockout-reject path (`AuthService::login` `is_locked` early return,
/// best-effort).
pub const AUTH_LOGIN_LOCKED: &str = "auth.login.locked";
/// Refresh success (`AuthService::refresh`). §2.7 pair token.
pub const AUTH_REFRESH: &str = "auth.refresh";
/// Personal Access Token minted (`PatRepo::create`, in-tx).
/// RENAMED from `auth.pat.create` (DP-1 — §2.7 is normative).
pub const AUTH_PAT_ISSUE: &str = "auth.pat.issue";
/// Personal Access Token revoked (`PatRepo::revoke`, in-tx, only when a
/// still-active owned row was revoked).
pub const AUTH_PAT_REVOKE: &str = "auth.pat.revoke";
/// TOTP enrollment activated (`TotpRepo::activate`, in-tx) / removed
/// (`TotpRepo::disable`, in-tx). REPLACES `auth.totp.enabled` /
/// `auth.totp.disabled` (DP-1). §2.7 pair token on the enroll/activate
/// route paths; the disable route stays pool-level (audit-only — outside
/// the §2.7 pair table).
pub const AUTH_TOTP_ENROLL: &str = "auth.totp.enroll";
/// Backup recovery-code batch regenerated (`RecoveryCodeRepo::generate`,
/// in-tx, unconditional — every call replaces the batch).
pub const AUTH_TOTP_RECOVERY_CODES_REGENERATED: &str = "auth.totp.recovery_codes_regenerated";
/// Login email address changed (`ParticipantRepo::update_email`, in-tx).
pub const AUTH_EMAIL_CHANGED: &str = "auth.email.changed";
/// Password changed by an authenticated participant
/// (`CredentialRotationRepo::change_password`, in-tx, `Applied` only).
pub const AUTH_PASSWORD_CHANGED: &str = "auth.password.changed";
/// Password reset via a consumed reset token
/// (`CredentialRotationRepo::reset_password`, in-tx, `Applied` only).
pub const AUTH_PASSWORD_RESET: &str = "auth.password.reset";
/// Session revoked by its owner (inventory revoke / logout — §2.7 pair
/// token; D6 continuity — kept for trajectory compatibility).
pub const SESSION_REVOKED: &str = "session.revoked";
/// Session revoked by a workspace owner/admin force-revoke (new, §2.7 pair
/// token — embedded in `admin_revoke.rs`'s existing transaction).
pub const SESSION_REVOKED_ADMIN: &str = "session.revoked.admin";

#[cfg(test)]
mod tests {
    use super::*;

    /// Server-owned audit tokens that stay OUTSIDE this vocabulary
    /// (untouched by this direction): the two existing success-side login
    /// signals emitted from the HTTP layer. (B5-1: `session.revoked` moved
    /// INTO the vocabulary — it is now a §2.7 pair token mirrored in
    /// aero-storage.)
    const SERVER_OWNED: [&str; 2] = ["auth.login.new_ip", "auth.login.recovery_code"];

    #[test]
    fn auth_tokens_are_pinned_verbatim() {
        // The storage emitters, the connector drill seeds, and the governance
        // parity fixture all write these literal strings (dependency
        // direction) — a rename here must fail loudly, never silently drift.
        assert_eq!(AUTH_REGISTER, "auth.register");
        assert_eq!(AUTH_LOGIN, "auth.login");
        assert_eq!(AUTH_LOGIN_FAILED, "auth.login.failed");
        assert_eq!(AUTH_LOGIN_LOCKED, "auth.login.locked");
        assert_eq!(AUTH_REFRESH, "auth.refresh");
        assert_eq!(AUTH_PAT_ISSUE, "auth.pat.issue");
        assert_eq!(AUTH_PAT_REVOKE, "auth.pat.revoke");
        assert_eq!(AUTH_TOTP_ENROLL, "auth.totp.enroll");
        assert_eq!(
            AUTH_TOTP_RECOVERY_CODES_REGENERATED,
            "auth.totp.recovery_codes_regenerated"
        );
        assert_eq!(AUTH_EMAIL_CHANGED, "auth.email.changed");
        assert_eq!(AUTH_PASSWORD_CHANGED, "auth.password.changed");
        assert_eq!(AUTH_PASSWORD_RESET, "auth.password.reset");
        assert_eq!(SESSION_REVOKED, "session.revoked");
        assert_eq!(SESSION_REVOKED_ADMIN, "session.revoked.admin");
    }

    #[test]
    fn auth_tokens_follow_the_dotted_prefix_and_are_pairwise_disjoint() {
        let all = [
            AUTH_REGISTER,
            AUTH_LOGIN,
            AUTH_LOGIN_FAILED,
            AUTH_LOGIN_LOCKED,
            AUTH_REFRESH,
            AUTH_PAT_ISSUE,
            AUTH_PAT_REVOKE,
            AUTH_TOTP_ENROLL,
            AUTH_TOTP_RECOVERY_CODES_REGENERATED,
            AUTH_EMAIL_CHANGED,
            AUTH_PASSWORD_CHANGED,
            AUTH_PASSWORD_RESET,
            SESSION_REVOKED,
            SESSION_REVOKED_ADMIN,
        ];
        for (i, token) in all.iter().enumerate() {
            assert!(
                token.starts_with("auth.") || token.starts_with("session."),
                "{token} must follow the auth.*/session.* dotted style"
            );
            assert!(
                !SERVER_OWNED.contains(token),
                "{token} collides with a server-owned token"
            );
            for other in &all[i + 1..] {
                assert_ne!(token, other, "vocabulary must be pairwise disjoint");
            }
        }
    }
}
