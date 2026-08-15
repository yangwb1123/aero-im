//! B5-1 auth-slice governance `db_tests`: §2.7 allowlist parity, no-miss-write,
//! and the F1 v1-coexistence / fail-open regression pair (§6 AC-2/AC-3).

use super::*;
use crate::audit_governance::outbox::AuditGovernanceOutboxRepo;
use crate::audit_governance::tokens::{
    AUTH_LOGIN, AUTH_PAT_ISSUE, AUTH_PAT_REVOKE, AUTH_REGISTER, AUTH_REFRESH, AUTH_SOURCE_SYSTEM,
    AUTH_TOTP_ENROLL, OUTBOUND_AUTH_LOGIN, OUTBOUND_AUTH_PAT_ISSUE, OUTBOUND_AUTH_PAT_REVOKE,
    OUTBOUND_AUTH_REGISTER, OUTBOUND_AUTH_REFRESH, OUTBOUND_AUTH_SESSION_REVOKE,
    OUTBOUND_AUTH_TOTP_ENROLL, SESSION_REVOKED, SESSION_REVOKED_ADMIN,
};
use crate::PatRepo;
use crate::TotpRepo;
use aero_common::{
    AUDIT_ACTOR_TYPE_PARTICIPANT, AUDIT_AGGREGATE_TYPE, AUDIT_DATA_CLASSIFICATION,
    AUDIT_EVENT_TYPE, AUDIT_OUTCOME_SUCCESS, AUDIT_RETENTION_CLASS, AUDIT_SCHEMA_ID,
    AUDIT_SCHEMA_VERSION, AUDIT_TARGET_TYPE_RESOURCE, GOVERNANCE_CLASS_ADMIN, SessionId,
};
use serde_json::json;

/// Envelope field-by-field assertion (G2 closure — recomputed from the leaf
/// consts, never inline literals). `occurred_at` is asserted semantically
/// (RFC3339-parses, equals the audit row's `created_at` within 2 µs).
#[allow(clippy::too_many_arguments)]
async fn assert_pair_envelope(
    p: &PgPool,
    audit_id: uuid::Uuid,
    ws: WorkspaceId,
    action_local: &str,
    outbound: &str,
    actor: Option<ParticipantId>,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    let row: (String, i32, String, i16, serde_json::Value) = sqlx::query_as(
        "SELECT event_id::text, status, class, priority, payload
           FROM audit_governance_outbox
          WHERE event_id = $1",
    )
    .bind(audit_id)
    .fetch_one(p)
    .await
    .expect("outbox row for the pair");
    assert_eq!(row.0, audit_id.to_string(), "event_id 1:1 with audit_events.id");
    assert_eq!(row.1, 0, "status 0 = enqueued (0239 normative)");
    assert_eq!(
        row.2, GOVERNANCE_CLASS_ADMIN,
        "class 'admin' (GOVERNANCE_CLASS_ADMIN cross-slice pin)"
    );
    assert_eq!(row.3, 10, "priority 10 = GOVERNANCE_PRIORITY_BACKLOG");
    let envelope = row.4;
    assert_eq!(envelope["event_id"], audit_id.to_string());
    assert_eq!(
        envelope["source_system"], AUTH_SOURCE_SYSTEM,
        "source_system = AUTH_SOURCE_SYSTEM (auth explicit writes)"
    );
    assert_eq!(envelope["event_type"], AUDIT_EVENT_TYPE);
    assert_eq!(envelope["schema_id"], AUDIT_SCHEMA_ID);
    assert_eq!(envelope["schema_version"], AUDIT_SCHEMA_VERSION);
    let occurred = envelope["occurred_at"]
        .as_str()
        .expect("occurred_at string");
    let parsed = time::OffsetDateTime::parse(
        occurred,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("occurred_at is RFC3339");
    let audit_created: time::OffsetDateTime = sqlx::query_scalar(
        "SELECT created_at FROM audit_events WHERE id = $1",
    )
    .bind(audit_id)
    .fetch_one(p)
    .await
    .expect("audit created_at");
    assert!(
        (parsed - audit_created).abs() < time::Duration::microseconds(2),
        "occurred_at mirrors the server-stamped audit created_at"
    );
    if let Some(actor) = actor {
        assert_eq!(envelope["actor"]["id"], actor.to_uuid().to_string());
        assert_eq!(envelope["actor"]["type"], AUDIT_ACTOR_TYPE_PARTICIPANT);
    } else {
        assert_eq!(envelope["actor"]["id"], "system");
        assert_eq!(envelope["actor"]["type"], "system");
    }
    if let Some(target) = target {
        assert_eq!(
            envelope["targets"],
            json!([{ "id": target, "type": AUDIT_TARGET_TYPE_RESOURCE }])
        );
    } else {
        assert_eq!(envelope["targets"], json!([]));
    }
    assert_eq!(envelope["aggregate_type"], AUDIT_AGGREGATE_TYPE);
    assert_eq!(envelope["aggregate_id"], ws.to_uuid().to_string());
    assert_eq!(
        envelope["action"], outbound,
        "payload action = the §2.7 outbound token"
    );
    assert_eq!(envelope["outcome"], AUDIT_OUTCOME_SUCCESS);
    assert_eq!(envelope["payload"], detail, "payload = the caller detail");
    assert_eq!(envelope["data_classification"], AUDIT_DATA_CLASSIFICATION);
    assert_eq!(envelope["retention_class"], AUDIT_RETENTION_CLASS);
    assert_eq!(
        envelope["idempotency_key"],
        audit_id.to_string(),
        "idempotency_key = event_id (sink dedup)"
    );
    assert!(
        envelope.get("aggregated").is_none(),
        "no aggregated key on 1:1 auth rows (parity-exemption key is L1-only)"
    );
    // The local token must be in the audit row (1:1 starting point).
    let local: String = sqlx::query_scalar("SELECT action FROM audit_events WHERE id = $1")
        .bind(audit_id)
        .fetch_one(p)
        .await
        .expect("audit row action");
    assert_eq!(local, action_local, "local token verbatim");
}

/// Register a participant via the actual route-shaped write path
/// (`RegistrationRepo::create` with `auth_audit: Some(...)` — the
/// `register_enrolled` construction). Returns (audit id, participant id).
async fn register_pair(p: &PgPool, ws: WorkspaceId) -> (uuid::Uuid, ParticipantId) {
    let participant = ParticipantId::new();
    let session = SessionId::new();
    crate::RegistrationRepo::new(p.clone())
        .create(crate::NewRegistration {
            participant_id: participant,
            email: format!("auth-parity-{participant}@example.test"),
            display_name: "Auth Parity".into(),
            password_hash: "test-hash".into(),
            workspace_id: ws,
            session_id: session,
            refresh_token_hash: format!("refresh-{session}"),
            user_agent: None,
            auth_audit: Some(crate::NewRegistrationAudit {
                action: AUTH_REGISTER,
                target: participant.to_string(),
                detail: json!({}),
                outbound_action: OUTBOUND_AUTH_REGISTER,
            }),
        })
        .await
        .expect("registration commits");
    let audit_id = sqlx::query_scalar(
        "SELECT id FROM audit_events WHERE actor_id = $1 AND action = $2",
    )
    .bind(participant.to_uuid())
    .bind(AUTH_REGISTER)
    .fetch_one(p)
    .await
    .expect("auth.register audit row");
    (audit_id, participant)
}

/// AC-2: §2.7 allowlist parity, 1:1, per token, driving the ACTUAL route-shaped
/// write paths: 1 audit + 1 outbox row with the 16-key envelope per token;
/// rollback leaves 0 (whole pair); same-id replay INSERT deduped (ON CONFLICT).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn auth_outbox_parity_1to1() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;

    // --- 1. auth.register (RegistrationRepo::create with auth_audit). ---
    let (register_audit, registered) = register_pair(&p, ws).await;
    assert_pair_envelope(
        &p,
        register_audit,
        ws,
        AUTH_REGISTER,
        OUTBOUND_AUTH_REGISTER,
        Some(registered),
        Some(&registered.to_string()),
        json!({}),
    )
    .await;

    // --- 2. auth.login (record_pair_standalone, handler-shaped). ---
    let login_audit = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_LOGIN,
            Some(&actor.to_string()),
            json!({}),
            OUTBOUND_AUTH_LOGIN,
        )
        .await
        .expect("login pair");
    assert_pair_envelope(
        &p,
        login_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_LOGIN,
        OUTBOUND_AUTH_LOGIN,
        Some(actor),
        Some(&actor.to_string()),
        json!({}),
    )
    .await;

    // --- 3. auth.refresh (record_pair_standalone, service-shaped). ---
    let refresh_audit = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_REFRESH,
            Some("session-refresh-1"),
            json!({}),
            OUTBOUND_AUTH_REFRESH,
        )
        .await
        .expect("refresh pair");
    assert_pair_envelope(
        &p,
        refresh_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_REFRESH,
        OUTBOUND_AUTH_REFRESH,
        Some(actor),
        Some("session-refresh-1"),
        json!({}),
    )
    .await;

    // --- 4. auth.pat.issue (route: begin → create_in_tx → pair → commit). ---
    let mut tx = p.begin().await.expect("begin pat tx");
    let pat_id = PatRepo::create_in_tx(
        &mut tx,
        actor,
        &crate::pat::hash_pat(&format!("auth-parity-pat-{actor}")),
        Some("parity"),
        &["read".to_owned()],
        None,
    )
    .await
    .expect("pat mint in tx");
    let pat_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_PAT_ISSUE,
        Some(&pat_id.to_string()),
        json!({ "scopes": ["read"] }),
        OUTBOUND_AUTH_PAT_ISSUE,
    )
    .await
    .expect("pat issue pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit pat tx");
    assert_pair_envelope(
        &p,
        pat_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_PAT_ISSUE,
        OUTBOUND_AUTH_PAT_ISSUE,
        Some(actor),
        Some(&pat_id.to_string()),
        json!({ "scopes": ["read"] }),
    )
    .await;

    // --- 5. auth.pat.revoke (route: begin → revoke_in_tx → pair → commit). ---
    let mut tx = p.begin().await.expect("begin pat revoke tx");
    let revoked = PatRepo::revoke_in_tx(&mut tx, pat_id, actor)
        .await
        .expect("pat revoke in tx");
    assert!(revoked, "owned active token revokes");
    let revoke_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_PAT_REVOKE,
        Some(&pat_id.to_string()),
        json!({}),
        OUTBOUND_AUTH_PAT_REVOKE,
    )
    .await
    .expect("pat revoke pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit pat revoke tx");
    assert_pair_envelope(
        &p,
        revoke_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_PAT_REVOKE,
        OUTBOUND_AUTH_PAT_REVOKE,
        Some(actor),
        Some(&pat_id.to_string()),
        json!({}),
    )
    .await;

    // --- 6. auth.totp.enroll (route: begin → upsert_secret_in_tx → pair). ---
    let mut tx = p.begin().await.expect("begin totp enroll tx");
    TotpRepo::upsert_secret_in_tx(&mut tx, actor, "JBSWY3DPEHPK3PXP")
        .await
        .expect("totp upsert in tx");
    let enroll_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_TOTP_ENROLL,
        Some(&actor.to_string()),
        json!({ "stage": "enroll" }),
        OUTBOUND_AUTH_TOTP_ENROLL,
    )
    .await
    .expect("totp enroll pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit totp enroll tx");
    assert_pair_envelope(
        &p,
        enroll_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_TOTP_ENROLL,
        OUTBOUND_AUTH_TOTP_ENROLL,
        Some(actor),
        Some(&actor.to_string()),
        json!({ "stage": "enroll" }),
    )
    .await;

    // --- 7. auth.totp.enroll (activate stage; pair only when a row flipped). ---
    let mut tx = p.begin().await.expect("begin totp activate tx");
    let activated = TotpRepo::activate_in_tx(&mut tx, actor)
        .await
        .expect("totp activate in tx");
    assert!(activated, "pending enrollment activates");
    let activate_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_TOTP_ENROLL,
        Some(&actor.to_string()),
        json!({ "stage": "activate" }),
        OUTBOUND_AUTH_TOTP_ENROLL,
    )
    .await
    .expect("totp activate pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit totp activate tx");
    assert_pair_envelope(
        &p,
        activate_audit.to_uuid(),
        WorkspaceId::nil(),
        AUTH_TOTP_ENROLL,
        OUTBOUND_AUTH_TOTP_ENROLL,
        Some(actor),
        Some(&actor.to_string()),
        json!({ "stage": "activate" }),
    )
    .await;

    // --- 8. session.revoked (user-side inventory revoke). ---
    let session = SessionId::new();
    sqlx::query(
        r"INSERT INTO auth_sessions
              (id, participant_id, token_hash, user_agent, created_at, last_seen_at)
           VALUES ($1, $2, $3, 'parity-device', now(), now())",
    )
    .bind(session.to_uuid())
    .bind(actor.to_uuid())
    .bind(format!("auth-parity-session-{session}"))
    .execute(&p)
    .await
    .expect("seed session");
    let mut tx = p.begin().await.expect("begin session revoke tx");
    let session_revoked = crate::SessionRepo::revoke_and_blacklist_in_tx(&mut tx, session, actor)
        .await
        .expect("session revoke in tx");
    assert!(session_revoked, "active owner session revokes");
    let session_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        SESSION_REVOKED,
        Some(&session.to_string()),
        json!({ "session_id": session.to_string() }),
        OUTBOUND_AUTH_SESSION_REVOKE,
    )
    .await
    .expect("session revoke pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit session revoke tx");
    assert_pair_envelope(
        &p,
        session_audit.to_uuid(),
        WorkspaceId::nil(),
        SESSION_REVOKED,
        OUTBOUND_AUTH_SESSION_REVOKE,
        Some(actor),
        Some(&session.to_string()),
        json!({ "session_id": session.to_string() }),
    )
    .await;

    // --- 9. session.revoked.admin (admin_revoke.rs embeds this exact call). ---
    let mut tx = p.begin().await.expect("begin admin revoke tx");
    let admin_audit = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        ws,
        Some(actor),
        SESSION_REVOKED_ADMIN,
        Some(&actor.to_string()),
        json!({ "admin_revoked": true }),
        OUTBOUND_AUTH_SESSION_REVOKE,
    )
    .await
    .expect("admin revoke pair")
    .expect("pair written (no fail-open)");
    tx.commit().await.expect("commit admin revoke tx");
    assert_pair_envelope(
        &p,
        admin_audit.to_uuid(),
        ws,
        SESSION_REVOKED_ADMIN,
        OUTBOUND_AUTH_SESSION_REVOKE,
        Some(actor),
        Some(&actor.to_string()),
        json!({ "admin_revoked": true }),
    )
    .await;

    // --- Rollback half: whole pair, zero rows (never a half pair). ---
    let mut tx = p.begin().await.expect("begin rollback tx");
    let rolled_back = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_LOGIN,
        Some(&actor.to_string()),
        json!({}),
        OUTBOUND_AUTH_LOGIN,
    )
    .await
    .expect("pair in rollback tx")
    .expect("pair written");
    // Visible INSIDE the tx (same connection — READ COMMITTED isolation).
    let tx_audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE id = $1")
        .bind(rolled_back.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let tx_outbox: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = $1")
            .bind(rolled_back.to_uuid())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!((tx_audit, tx_outbox), (1, 1), "pair visible inside the tx");
    tx.rollback().await.expect("rollback");
    let tx_audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE id = $1")
        .bind(rolled_back.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
    let tx_outbox: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = $1")
            .bind(rolled_back.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(
        (tx_audit, tx_outbox),
        (0, 0),
        "rollback removes the WHOLE pair — zero rows escape (R1)"
    );

    // --- Replay half: same-id outbox INSERT deduped (0239 ON CONFLICT). ---
    let mut replay_tx = p.begin().await.expect("begin replay tx");
    AuditGovernanceOutboxRepo::append_in_tx(
        &mut replay_tx,
        login_audit,
        GOVERNANCE_CLASS_ADMIN,
        10,
        json!({}),
    )
    .await
    .expect("duplicate outbox insert");
    replay_tx.commit().await.expect("commit replay tx");
    let deduped: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = $1")
            .bind(login_audit.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(deduped, 1, "same-id replay INSERT is deduped (ON CONFLICT DO NOTHING)");
}

/// AC-3: under enforcement ON + binding, exercising every §2.7 producer leaves
/// ZERO allowlist-token audit rows without their outbox row (no-miss-write);
/// scoped to this test's workspaces, allowlist literal verbatim (AC2).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn auth_allowlist_no_miss_write() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Pre-clean this slice's OWN allowlist-token audit rows in the producer
    // workspaces: repeated suite runs on one DB leave paired rows behind
    // whose outbox rows the reset just deleted — without the pre-clean they
    // would look like orphans to the verbatim no-miss-write scan below
    // (fresh harness DBs are unaffected; this keeps re-runs honest).
    sqlx::query(
        r"DELETE FROM audit_events
           WHERE workspace_id IN ($1, $2)
             AND action IN ('message.moderated','auth.register','auth.login',
                            'auth.refresh','auth.pat.issue','auth.pat.revoke',
                            'auth.totp.enroll','session.revoked',
                            'session.revoked.admin')",
    )
    .bind(ws.to_uuid())
    .bind(WorkspaceId::nil().to_uuid())
    .execute(&p)
    .await
    .expect("pre-clean allowlist audit rows");
    // Enforcement + bindings for BOTH producer workspaces
    // (register → fixture ws; account-level tokens → nil default).
    enable_enforcement_with_binding(&p, ws).await;
    enable_enforcement_with_binding(&p, WorkspaceId::nil()).await;

    // Exercise every §2.7 producer (the same route-shaped calls
    // auth_outbox_parity_1to1 drives; every row below is paired).
    register_pair(&p, ws).await;
    let _ = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_LOGIN,
            Some(&actor.to_string()),
            json!({}),
            OUTBOUND_AUTH_LOGIN,
        )
        .await
        .expect("login pair");
    let _ = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_REFRESH,
            Some("session-nmw-1"),
            json!({}),
            OUTBOUND_AUTH_REFRESH,
        )
        .await
        .expect("refresh pair");
    let mut tx = p.begin().await.expect("begin pat tx");
    let pat_id = PatRepo::create_in_tx(
        &mut tx,
        actor,
        &crate::pat::hash_pat(&format!("auth-nmw-pat-{actor}")),
        None,
        &["read".to_owned()],
        None,
    )
    .await
    .expect("pat mint");
    let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_PAT_ISSUE,
        Some(&pat_id.to_string()),
        json!({ "scopes": ["read"] }),
        OUTBOUND_AUTH_PAT_ISSUE,
    )
    .await
    .expect("pat issue pair");
    let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_PAT_REVOKE,
        Some(&pat_id.to_string()),
        json!({}),
        OUTBOUND_AUTH_PAT_REVOKE,
    )
    .await
    .expect("pat revoke pair");
    TotpRepo::upsert_secret_in_tx(&mut tx, actor, "JBSWY3DPEHPK3PXP")
        .await
        .expect("totp upsert in tx");
    let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        AUTH_TOTP_ENROLL,
        Some(&actor.to_string()),
        json!({ "stage": "enroll" }),
        OUTBOUND_AUTH_TOTP_ENROLL,
    )
    .await
    .expect("totp enroll pair");
    let session = SessionId::new();
    sqlx::query(
        r"INSERT INTO auth_sessions
              (id, participant_id, token_hash, user_agent, created_at, last_seen_at)
           VALUES ($1, $2, $3, 'nmw-device', now(), now())",
    )
    .bind(session.to_uuid())
    .bind(actor.to_uuid())
    .bind(format!("auth-nmw-session-{session}"))
    .execute(&mut *tx)
    .await
    .expect("seed session in tx");
    let _ = crate::SessionRepo::revoke_and_blacklist_in_tx(&mut tx, session, actor)
        .await
        .expect("session revoke");
    let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        WorkspaceId::nil(),
        Some(actor),
        SESSION_REVOKED,
        Some(&session.to_string()),
        json!({ "session_id": session.to_string() }),
        OUTBOUND_AUTH_SESSION_REVOKE,
    )
    .await
    .expect("session revoke pair");
    let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        ws,
        Some(actor),
        SESSION_REVOKED_ADMIN,
        Some(&actor.to_string()),
        json!({ "admin_revoked": true }),
        OUTBOUND_AUTH_SESSION_REVOKE,
    )
    .await
    .expect("admin revoke pair");
    tx.commit().await.expect("commit producer tx");

    // The verbatim §2.7 allowlist no-miss-write query (connector design §6
    // AC2), scoped to this test's workspaces: zero = fully paired.
    let orphans: i64 = sqlx::query_scalar(
        r"SELECT count(*) FROM audit_events a
           WHERE a.workspace_id IN ($1, $2)
             AND a.action IN ('message.moderated','auth.register','auth.login',
                              'auth.refresh','auth.pat.issue','auth.pat.revoke',
                              'auth.totp.enroll','session.revoked',
                              'session.revoked.admin')
             AND NOT EXISTS (SELECT 1 FROM audit_governance_outbox o
                              WHERE o.event_id = a.id)",
    )
    .bind(ws.to_uuid())
    .bind(WorkspaceId::nil().to_uuid())
    .fetch_one(&p)
    .await
    .expect("no-miss-write count");
    assert_eq!(
        orphans, 0,
        "no allowlist-token audit row without its outbox row (no-miss-write)"
    );

    restore_enforcement_disabled(&p).await;
}

/// F1(a): enforcement ON + a nil-workspace enabled binding → the auth pair
/// commits 1+1 AND the 0236 v1 trigger produces exactly one
/// `snaplink_delivery_outbox` row (v1/v2 coexistence — v1 path untouched).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn auth_pair_enforcement_on_with_nil_binding_commits_pair_and_v1() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Enforcement ON + binding for the NIL workspace (account-level writes;
    // the fixture ws needs no binding).
    enable_enforcement_with_binding(&p, WorkspaceId::nil()).await;

    let audit_id = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_LOGIN,
            Some(&actor.to_string()),
            json!({}),
            OUTBOUND_AUTH_LOGIN,
        )
        .await
        .expect("login pair commits under enforcement + nil binding");

    // Pair 1+1 (envelope asserted field-by-field).
    assert_pair_envelope(
        &p,
        audit_id.to_uuid(),
        WorkspaceId::nil(),
        AUTH_LOGIN,
        OUTBOUND_AUTH_LOGIN,
        Some(actor),
        Some(&actor.to_string()),
        json!({}),
    )
    .await;

    // v1 coexistence: exactly one 0236 row for the same audit id (the v1
    // relay keeps flowing — v1/v2 dual-path during the cutover window).
    let v1: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
    )
    .bind(audit_id.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("count v1 rows")
    .0;
    assert_eq!(v1, 1, "v1 audit delivery row coexists (0236 trigger untouched)");
    let v1_action: String = sqlx::query_scalar(
        "SELECT payload->>'action' FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
    )
    .bind(audit_id.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("v1 payload action");
    assert_eq!(v1_action, AUTH_LOGIN, "v1 keeps the local token");

    let _ = ws; // fixture ws deliberately unused beyond fixture setup
    restore_enforcement_disabled(&p).await;
}

/// F1(b): enforcement ON with NO nil-workspace binding (control-plane-
/// unreachable state, direct fixture flip) → the pair fails OPEN: `None`,
/// zero audit/outbox rows, exactly one DLQ row (`error_sqlstate='P0001'`),
/// forced counter `{category="binding"}` ≥ 1.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn auth_pair_enforcement_on_without_nil_binding_fails_open_drops_pair() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Direct flip (control-plane-unreachable state — configure_enabled's
    // require_complete_workspace_coverage would reject it). The fixture
    // consumer of the designed-only has_enabled_binding helper (D13) pins the
    // 0236 trigger's binding-lookup state (enforcement on, NO nil-workspace
    // binding → RAISE P0001). Earlier tests in the same run
    // (auth_pair_enforcement_on_with_nil_binding_*) leave the nil binding
    // row behind (restore only disables the switch) — delete it first so the
    // precondition is order-independent.
    sqlx::query(
        "DELETE FROM snaplink_commercial_bindings WHERE workspace_id = $1",
    )
    .bind(WorkspaceId::nil().to_uuid())
    .execute(&p)
    .await
    .expect("clear nil-workspace bindings");
    sqlx::query(
        "UPDATE snaplink_commercial_runtime SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("flip enforcement on without bindings");
    assert!(
        !crate::snaplink_commercial::SnaplinkCommercialRepo::new(p.clone())
            .has_enabled_binding(WorkspaceId::nil())
            .await
            .expect("binding lookup"),
        "fixture state: enforcement on with NO nil-workspace binding (the P0001 precondition)"
    );

    let counter_before = binding_counter();
    let pair = AuditGovernanceOutboxRepo::new(p.clone())
        .record_pair_standalone(
            WorkspaceId::nil(),
            Some(actor),
            AUTH_LOGIN,
            Some(&actor.to_string()),
            json!({}),
            OUTBOUND_AUTH_LOGIN,
        )
        .await;
    assert!(pair.is_none(), "P0001 → fail-open: Ok(None)");

    let audit_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = $1 AND actor_id = $2",
    )
    .bind(AUTH_LOGIN)
    .bind(actor.to_uuid())
    .fetch_one(&p)
    .await
    .expect("count audit rows");
    assert_eq!(audit_rows, 0, "whole pair absent — zero audit rows (R7)");
    let outbox_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox")
            .fetch_one(&p)
            .await
            .expect("count outbox rows");
    assert_eq!(outbox_rows, 0, "whole pair absent — zero outbox rows (R7)");
    let dlq: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, action, error_sqlstate FROM audit_governance_failed_pairs",
    )
    .fetch_all(&p)
    .await
    .expect("dlq rows");
    assert_eq!(dlq.len(), 1, "exactly one DLQ row (replayable, not lost)");
    assert_eq!(dlq[0].1, AUTH_LOGIN, "DLQ carries the original action");
    assert_eq!(dlq[0].2, "P0001", "error_sqlstate = the 0236 binding RAISE");
    let counter_after = binding_counter();
    assert!(
        counter_after > counter_before,
        "forced counter increment (>= semantics, process-global registry): \
         {counter_before} -> {counter_after}"
    );

    let _ = ws;
    restore_enforcement_disabled(&p).await;
}

/// The `audit_auth_write_failures_total{category="binding"}` counter value
/// from the process-global Prometheus registry (rendered text — the only
/// introspection surface for the storage-emitted counter).
fn binding_counter() -> u64 {
    let rendered = aero_common::metrics::render_prometheus();
    let needle = "audit_auth_write_failures_total{category=\"binding\"} ";
    rendered
        .lines()
        .find_map(|line| {
            line.strip_prefix(needle)
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .unwrap_or(0)
}
