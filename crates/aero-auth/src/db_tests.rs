//! PG-gated integration tests for the auth security-event audit (AC2).
//!
//! Run with a live Postgres + applied migrations:
//!
//! ```text
//! DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
//!   cargo test -p aero-auth --lib db_tests:: -- --ignored --test-threads=1
//! ```
//!
//! `#[ignore]` so the default `cargo test` stays hermetic (no DB in CI).
//!
//! Pins the login-path audit contract (design §2.2 / AC2):
//! * every `Unauthorized` outcome of `AuthService::login` emits exactly one
//!   `auth.login.failed` row (`reason = "invalid_credentials"`), whether or
//!   not the login throttle is configured (F8 regression guard);
//! * the lockout-reject path emits exactly one `auth.login.locked` row
//!   (`reason = "locked"`) and does NOT also emit a failed row;
//! * `finalize_login(account, false)` (failed 2FA) emits exactly one
//!   `auth.login.failed` row (`reason = "2fa_failed"`), and the two reason
//!   tokens never co-occur for one attempt (double-emission guard mirroring
//!   the routes' no-finalize-after-Err shape);
//! * successes are silent (`finalize_login(account, true)`, correct
//!   password) — durable `login_events` covers the success side;
//! * the audit write never alters the login outcome (best-effort `let _`).
//!
//! All count queries are scoped `WHERE detail->>'email' = $account` — the
//! mod runs several tests per shared throwaway DB (`--test-threads=1` keeps
//! per-account isolation deterministic).

use std::sync::Arc;
use std::time::Duration;

use aero_common::{Error, ParticipantId, WorkspaceId};
use aero_storage::ParticipantRepo;
use sqlx::PgPool;

use crate::audit_tokens::{AUTH_LOGIN_FAILED, AUTH_LOGIN_LOCKED};
use crate::jwt::JwtCodec;
use crate::login_throttle::{LockoutConfig, LoginThrottle};
use crate::service::{AuthService, LoginRequest, RegisterRequest};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

/// Service with a freshly generated RSA keypair (the login path issues real
/// JWTs, so the codec must be functional). `throttle`: `Some` enables the
/// in-process per-account lockout, `None` leaves it disabled.
fn service(pool: PgPool, throttle: Option<LoginThrottle>) -> AuthService {
    use rand::rngs::OsRng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;

    let mut rng = OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
    let private_pem = private
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .unwrap()
        .to_string();
    let public_pem = private.to_public_key().to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let jwt =
        JwtCodec::from_pem(&private_pem, &public_pem, "aero-im", Duration::from_secs(60), Duration::from_secs(600))
            .expect("jwt codec");
    let mut svc = AuthService::new(ParticipantRepo::new(pool), jwt);
    if let Some(throttle) = throttle {
        svc = svc.with_login_throttle(Arc::new(throttle));
    }
    svc
}

/// Register a fresh account (via the real `register_enrolled` seam, in the
/// nil default workspace — the 0006-seeded account-level tenant), returning
/// its id and email.
async fn register_account(svc: &AuthService, label: &str) -> (ParticipantId, String) {
    let pid = ParticipantId::new();
    let email = format!("{label}-{pid}@audit.test");
    svc.register_enrolled(
        RegisterRequest {
            email: email.clone(),
            password: "correct-horse-battery".into(),
            display_name: format!("audit-{label}"),
        },
        WorkspaceId::nil(),
        Some("db_tests"),
    )
    .await
    .expect("register_enrolled commits");
    (pid, email)
}

/// Count `auth.login.failed` rows for `email` (optionally by reason token).
async fn failed_rows(p: &PgPool, email: &str, reason: Option<&str>) -> i64 {
    match reason {
        Some(reason) => sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_events \
              WHERE action = $1 AND detail->>'email' = $2 AND detail->>'reason' = $3",
        )
        .bind(AUTH_LOGIN_FAILED)
        .bind(email)
        .bind(reason)
        .fetch_one(p)
        .await
        .expect("count failed rows by reason"),
        None => sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_events \
              WHERE action = $1 AND detail->>'email' = $2",
        )
        .bind(AUTH_LOGIN_FAILED)
        .bind(email)
        .fetch_one(p)
        .await
        .expect("count failed rows"),
    }
}

/// Count `auth.login.locked` rows for `email`.
async fn locked_rows(p: &PgPool, email: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_events \
          WHERE action = $1 AND detail->>'email' = $2",
    )
    .bind(AUTH_LOGIN_LOCKED)
    .bind(email)
    .fetch_one(p)
    .await
    .expect("count locked rows")
}

/// Cleanup (FK NO ACTION, 0007): the register audit row's `actor_id` is the
/// participant, so audit rows must go before the participant.
async fn cleanup(p: &PgPool, participant: ParticipantId) {
    sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("delete audit rows for the participant");
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("delete participant");
}

/// AC2 cases 1/2/5: wrong password → `Err(Unauthorized)` + exactly one
/// `auth.login.failed` row; a locked account (in-process throttle,
/// `max_failures: 1` — the first failure trips the lock) → `Err(Unauthorized)`
/// + exactly one `auth.login.locked` row and NO extra failed row; the audit
/// never alters the outcome.
#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn failed_login_and_lockout_audit_rows() {
    let p = pool();
    let throttle = LoginThrottle::new(LockoutConfig {
        max_failures: 1,
        window_secs: 300,
        lockout_secs: 900,
    });
    let svc = service(p.clone(), Some(throttle));
    let (pid, email) = register_account(&svc, "lockout").await;

    // (1) wrong password with the throttle configured → failed row, outcome
    // unchanged (the no-audit baseline is also Err(Unauthorized) here).
    let wrong = LoginRequest {
        email: email.clone(),
        password: "wrong-password".into(),
    };
    let err = svc.login(wrong.clone()).await.expect_err("wrong password");
    assert!(matches!(err, Error::Unauthorized(_)), "outcome unchanged by the audit");
    assert_eq!(
        failed_rows(&p, &email, Some("invalid_credentials")).await,
        1,
        "exactly one auth.login.failed row for the bad password"
    );
    assert_eq!(
        locked_rows(&p, &email).await,
        0,
        "no locked row on a plain failure"
    );

    // (2) max_failures 1 ⇒ the failure above tripped the lock; the next
    // attempt is rejected on the lockout path → locked row, no extra failed
    // row (the locked path returns before the failed emission).
    let err = svc.login(wrong).await.expect_err("locked account");
    assert!(matches!(err, Error::Unauthorized(_)), "outcome unchanged by the audit");
    assert_eq!(
        locked_rows(&p, &email).await,
        1,
        "exactly one auth.login.locked row for the lockout reject"
    );
    assert_eq!(
        failed_rows(&p, &email, Some("invalid_credentials")).await,
        1,
        "the locked path does not double-emit auth.login.failed"
    );

    cleanup(&p, pid).await;
}

/// AC2 case 4: the throttle-less early return must NOT skip the audit —
/// wrong password without any throttle configured still emits exactly one
/// `auth.login.failed` row (F8 regression guard).
#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn failed_login_audits_without_throttle() {
    let p = pool();
    let svc = service(p.clone(), None);
    let (pid, email) = register_account(&svc, "nothrottle").await;

    let err = svc
        .login(LoginRequest {
            email: email.clone(),
            password: "wrong-password".into(),
        })
        .await
        .expect_err("wrong password");
    assert!(matches!(err, Error::Unauthorized(_)), "outcome unchanged by the audit");
    assert_eq!(
        failed_rows(&p, &email, Some("invalid_credentials")).await,
        1,
        "failed login audits whether or not the throttle is configured"
    );
    assert_eq!(locked_rows(&p, &email).await, 0);

    cleanup(&p, pid).await;
}

/// AC2 cases 3/6/7/8: success is silent; `finalize_login(account, false)`
/// (failed 2FA) emits exactly one `auth.login.failed` row with
/// `reason = "2fa_failed"` and zero `invalid_credentials` rows — the
/// per-attempt total is exactly one (double-emission guard).
#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn failed_2fa_audits_and_success_is_silent() {
    let p = pool();
    let svc = service(p.clone(), None);
    let (pid, email) = register_account(&svc, "twofa").await;
    let correct = LoginRequest {
        email: email.clone(),
        password: "correct-horse-battery".into(),
    };

    // (6) correct password → Ok, zero failure rows.
    assert!(svc.login(correct.clone()).await.is_ok());
    assert_eq!(failed_rows(&p, &email, None).await, 0, "success is silent");
    assert_eq!(locked_rows(&p, &email).await, 0);

    // (7) finalize_login(true) after the success → still zero rows.
    svc.finalize_login(&email, true).await;
    assert_eq!(failed_rows(&p, &email, None).await, 0);
    assert_eq!(locked_rows(&p, &email).await, 0);

    // (3)/(8) failed 2FA: the password gate passed (login Ok), the 2FA gate
    // rejected (finalize_login(false)) → exactly one failed row with the
    // 2FA reason, zero invalid-credentials rows (the two reason tokens never
    // co-occur for one attempt).
    svc.finalize_login(&email, false).await;
    assert_eq!(
        failed_rows(&p, &email, Some("2fa_failed")).await,
        1,
        "failed 2FA emits exactly one auth.login.failed row"
    );
    assert_eq!(
        failed_rows(&p, &email, Some("invalid_credentials")).await,
        0,
        "a 2FA failure never also emits invalid_credentials"
    );
    assert_eq!(failed_rows(&p, &email, None).await, 1, "per-attempt total == 1");

    cleanup(&p, pid).await;
}

/// AC2 case 9: a wrong password emits exactly one `invalid_credentials` row
/// and zero `2fa_failed` rows — the password-failure shape of the routes'
/// no-finalize-after-Err flow, pinned at the service level.
#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn wrong_password_never_emits_2fa_reason() {
    let p = pool();
    let svc = service(p.clone(), None);
    let (pid, email) = register_account(&svc, "wrongpass").await;

    let err = svc
        .login(LoginRequest {
            email: email.clone(),
            password: "wrong-password".into(),
        })
        .await
        .expect_err("wrong password");
    assert!(matches!(err, Error::Unauthorized(_)));
    assert_eq!(
        failed_rows(&p, &email, Some("invalid_credentials")).await,
        1
    );
    assert_eq!(
        failed_rows(&p, &email, Some("2fa_failed")).await,
        0,
        "a password failure never emits the 2FA reason"
    );
    assert_eq!(failed_rows(&p, &email, None).await, 1);

    cleanup(&p, pid).await;
}
