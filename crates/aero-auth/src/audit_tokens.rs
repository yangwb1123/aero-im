//! Auth security-event audit action tokens — the single Rust source.
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
//! Workspace convention: account-level events land in
//! [`WorkspaceId::nil()`](aero_common::WorkspaceId::nil) (the 0006-seeded
//! "legacy / default" tenant — the established account-level security event
//! home, see the `auth.login.new_ip` handler comment); `auth.register` uses
//! the registration's actual `workspace_id`.

/// Account created by first-party registration (`RegistrationRepo::create`,
/// in-tx; also reused by SSO/OIDC JIT provisioning with
/// `detail.provisioning = "sso_jit"`).
pub const AUTH_REGISTER: &str = "auth.register";
/// Failed password / unknown email (`AuthService::login`, best-effort).
pub const AUTH_LOGIN_FAILED: &str = "auth.login.failed";
/// Lockout-reject path (`AuthService::login` `is_locked` early return,
/// best-effort).
pub const AUTH_LOGIN_LOCKED: &str = "auth.login.locked";
/// Personal Access Token minted (`PatRepo::create`, in-tx).
pub const AUTH_PAT_CREATE: &str = "auth.pat.create";
/// Personal Access Token revoked (`PatRepo::revoke`, in-tx, only when a
/// still-active owned row was revoked).
pub const AUTH_PAT_REVOKE: &str = "auth.pat.revoke";
/// TOTP enrollment activated (`TotpRepo::activate`, in-tx).
pub const AUTH_TOTP_ENABLED: &str = "auth.totp.enabled";
/// TOTP enrollment removed (`TotpRepo::disable`, in-tx).
pub const AUTH_TOTP_DISABLED: &str = "auth.totp.disabled";
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Server- and storage-owned audit tokens that stay OUTSIDE this
    /// vocabulary (untouched by this direction): the two existing
    /// success-side login signals emitted from the HTTP layer, and the
    /// session-revocation token emitted by the session lifecycle.
    const SERVER_OWNED: [&str; 3] = [
        "auth.login.new_ip",
        "auth.login.recovery_code",
        "session.revoked",
    ];

    #[test]
    fn auth_tokens_are_pinned_verbatim() {
        // The storage emitters, the connector drill seeds, and the governance
        // parity fixture all write these literal strings (dependency
        // direction) — a rename here must fail loudly, never silently drift.
        assert_eq!(AUTH_REGISTER, "auth.register");
        assert_eq!(AUTH_LOGIN_FAILED, "auth.login.failed");
        assert_eq!(AUTH_LOGIN_LOCKED, "auth.login.locked");
        assert_eq!(AUTH_PAT_CREATE, "auth.pat.create");
        assert_eq!(AUTH_PAT_REVOKE, "auth.pat.revoke");
        assert_eq!(AUTH_TOTP_ENABLED, "auth.totp.enabled");
        assert_eq!(AUTH_TOTP_DISABLED, "auth.totp.disabled");
        assert_eq!(
            AUTH_TOTP_RECOVERY_CODES_REGENERATED,
            "auth.totp.recovery_codes_regenerated"
        );
        assert_eq!(AUTH_EMAIL_CHANGED, "auth.email.changed");
        assert_eq!(AUTH_PASSWORD_CHANGED, "auth.password.changed");
        assert_eq!(AUTH_PASSWORD_RESET, "auth.password.reset");
    }

    #[test]
    fn auth_tokens_follow_the_dotted_prefix_and_are_pairwise_disjoint() {
        let all = [
            AUTH_REGISTER,
            AUTH_LOGIN_FAILED,
            AUTH_LOGIN_LOCKED,
            AUTH_PAT_CREATE,
            AUTH_PAT_REVOKE,
            AUTH_TOTP_ENABLED,
            AUTH_TOTP_DISABLED,
            AUTH_TOTP_RECOVERY_CODES_REGENERATED,
            AUTH_EMAIL_CHANGED,
            AUTH_PASSWORD_CHANGED,
            AUTH_PASSWORD_RESET,
        ];
        for (i, token) in all.iter().enumerate() {
            assert!(
                token.starts_with("auth."),
                "{token} must follow the auth.* dotted style"
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
