use super::{NewHuman, ParticipantRepo};
use aero_common::ParticipantId;
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

/// `credentials.email` is `citext`, so a login must match regardless of case.
/// A `citext = text` bind silently compares case-sensitively. This guards the
/// `$1::citext` cast in `find_credentials_by_email`.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn find_credentials_by_email_is_case_insensitive() {
    let repo = ParticipantRepo::new(pool());
    let suffix = ParticipantId::new();
    let stored = format!("MixedCase+{suffix}@Example.COM");
    let created = repo
        .create_human(NewHuman {
            email: format!("   {stored}\t"),
            display_name: "Case Test".into(),
            password_hash: "x".into(),
        })
        .await
        .expect("create_human");

    for variant in [
        stored.clone(),
        stored.to_lowercase(),
        stored.to_uppercase(),
        format!("  {stored}  "),
        format!("\t{}\n", stored.to_lowercase()),
    ] {
        let found = repo
            .find_credentials_by_email(&variant)
            .await
            .expect("lookup")
            .unwrap_or_else(|| {
                panic!("email lookup must be case/whitespace-insensitive (failed for {variant:?})")
            });
        assert_eq!(
            found.participant_id, created.id,
            "variant {variant:?} matched the wrong (or no) account",
        );
    }
}
