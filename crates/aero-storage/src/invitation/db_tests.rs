use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn new_participant(p: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("invite-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    id
}

async fn new_workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
    let ws = WorkspaceId::new();
    let mut tx = p.begin().await.expect("begin workspace fixture");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by, created_at) \
             VALUES ($1, $2, $3, $4, now())",
    )
    .bind(ws.to_uuid())
    .bind("Invite Test WS")
    .bind(format!("inv-{ws}"))
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace owner");
    tx.commit().await.expect("commit workspace fixture");
    ws
}

async fn use_count(p: &PgPool, invitation: InvitationId) -> i32 {
    sqlx::query_scalar("SELECT use_count FROM invitations WHERE id = $1")
        .bind(invitation.to_uuid())
        .fetch_one(p)
        .await
        .expect("read invitation use count")
}

async fn membership_role(
    p: &PgPool,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Option<String> {
    sqlx::query_scalar(
        r"SELECT role FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(p)
    .await
    .expect("read workspace membership")
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_accept_is_atomic_idempotent_and_respects_removal() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let invitee = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;
    let hash = hash_token(&generate_token());
    let id = repo
        .create(
            ws,
            &hash,
            None,
            WorkspaceRole::Member,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .expect("create single-use invite");

    let first = repo
        .accept_by_token_hash(&hash, invitee)
        .await
        .expect("first accept");
    assert!(!first.already_accepted);
    assert!(first.membership_created);
    assert!(first.membership_active);
    assert_eq!(first.role, WorkspaceRole::Member);
    assert_eq!(
        membership_role(&p, ws, invitee).await.as_deref(),
        Some("member")
    );
    assert_eq!(use_count(&p, id).await, 1);

    // The durable redemption precedes the active-state check, so retrying a
    // now-exhausted single-use invite succeeds without consuming another use.
    let repeated = repo
        .accept_by_token_hash(&hash, invitee)
        .await
        .expect("idempotent retry");
    assert!(repeated.already_accepted);
    assert!(repeated.membership_created);
    assert!(repeated.membership_active);
    assert_eq!(use_count(&p, id).await, 1);

    // An administrative removal is authoritative. A later transport retry
    // reports the inactive membership and must not silently re-enrol.
    sqlx::query(
        r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(ws.to_uuid())
    .bind(invitee.to_uuid())
    .execute(&p)
    .await
    .expect("remove membership");
    let after_removal = repo
        .accept_by_token_hash(&hash, invitee)
        .await
        .expect("durable retry after removal");
    assert!(after_removal.already_accepted);
    assert!(!after_removal.membership_active);
    assert!(membership_role(&p, ws, invitee).await.is_none());
    assert_eq!(use_count(&p, id).await, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_accept_existing_member_preserves_role_and_capacity() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let existing = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;
    sqlx::query(
        r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
              VALUES ($1, $2, 'admin', now())",
    )
    .bind(ws.to_uuid())
    .bind(existing.to_uuid())
    .execute(&p)
    .await
    .expect("insert existing administrator");
    let hash = hash_token(&generate_token());
    let id = repo
        .create(
            ws,
            &hash,
            None,
            WorkspaceRole::Owner,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .expect("create owner invite");

    let accepted = repo
        .accept_by_token_hash(&hash, existing)
        .await
        .expect("existing member accepts");
    assert!(!accepted.already_accepted);
    assert!(!accepted.membership_created);
    assert_eq!(accepted.role, WorkspaceRole::Admin);
    assert_eq!(
        membership_role(&p, ws, existing).await.as_deref(),
        Some("admin"),
        "an invitation must not overwrite or escalate an existing role"
    );
    assert_eq!(
        use_count(&p, id).await,
        0,
        "a no-op admission does not consume capacity"
    );

    let repeated = repo
        .accept_by_token_hash(&hash, existing)
        .await
        .expect("existing-member retry");
    assert!(repeated.already_accepted);
    assert!(!repeated.membership_created);
    assert_eq!(use_count(&p, id).await, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_invitation_accepts_last_slot_admit_exactly_one_member() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let first = new_participant(&p).await;
    let second = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;
    let hash = hash_token(&generate_token());
    let id = repo
        .create(
            ws,
            &hash,
            None,
            WorkspaceRole::Guest,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .expect("create last-slot invite");

    let (a, b) = tokio::join!(
        repo.accept_by_token_hash(&hash, first),
        repo.accept_by_token_hash(&hash, second)
    );
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "exactly one concurrent contender gets the final slot"
    );
    assert_eq!(
        usize::from(matches!(a, Err(InvitationAcceptError::NotRedeemable)))
            + usize::from(matches!(b, Err(InvitationAcceptError::NotRedeemable))),
        1,
        "the capacity loser receives the deterministic exhausted result"
    );
    assert_eq!(use_count(&p, id).await, 1);

    let first_role = membership_role(&p, ws, first).await;
    let second_role = membership_role(&p, ws, second).await;
    assert_eq!(
        usize::from(first_role.is_some()) + usize::from(second_role.is_some()),
        1,
        "the loser must not retain a membership side effect"
    );
    assert!(first_role
        .iter()
        .chain(second_role.iter())
        .all(|role| role == "guest"));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_accept_unredeemable_states_leave_no_membership() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let invitee = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;

    let revoked_hash = hash_token(&generate_token());
    let revoked = repo
        .create(
            ws,
            &revoked_hash,
            None,
            WorkspaceRole::Member,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .unwrap();
    assert!(repo.revoke(revoked).await.unwrap());

    let expired_hash = hash_token(&generate_token());
    repo.create(
        ws,
        &expired_hash,
        None,
        WorkspaceRole::Member,
        Some(owner),
        Some(1),
        Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
    )
    .await
    .unwrap();

    let exhausted_hash = hash_token(&generate_token());
    let exhausted = repo
        .create(
            ws,
            &exhausted_hash,
            None,
            WorkspaceRole::Member,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .unwrap();
    assert!(repo.increment_use(exhausted).await.unwrap());

    for hash in [&revoked_hash, &expired_hash, &exhausted_hash] {
        assert!(matches!(
            repo.accept_by_token_hash(hash, invitee).await,
            Err(InvitationAcceptError::NotRedeemable)
        ));
        assert!(
            membership_role(&p, ws, invitee).await.is_none(),
            "every rejected state must roll back/avoid membership"
        );
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_create_find_increment_then_exhaust() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;

    let token = generate_token();
    let hash = hash_token(&token);
    let id = repo
        .create(
            ws,
            &hash,
            Some("a@example.com"),
            WorkspaceRole::Member,
            Some(owner),
            Some(1),
            None,
        )
        .await
        .expect("create invite");

    let now = OffsetDateTime::now_utc();
    // Found while active, with its fields intact.
    let found = repo
        .find_active_by_token_hash(&hash, now)
        .await
        .unwrap()
        .expect("active invite resolves");
    assert_eq!(found.id, id);
    assert_eq!(found.workspace_id, ws);
    assert_eq!(found.role, WorkspaceRole::Member);
    assert_eq!(found.email.as_deref(), Some("a@example.com"));
    assert_eq!(found.max_uses, Some(1));
    assert_eq!(found.use_count, 0);

    // First redemption succeeds; the single-use invite is now exhausted.
    assert!(repo.increment_use(id).await.unwrap(), "first use recorded");
    // A second increment finds nothing (cap reached) → false.
    assert!(
        !repo.increment_use(id).await.unwrap(),
        "exhausted invite increments nothing"
    );
    // And it no longer resolves as active.
    assert!(
        repo.find_active_by_token_hash(&hash, now)
            .await
            .unwrap()
            .is_none(),
        "exhausted invite is not active"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_revoke_makes_it_inactive() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;

    let token = generate_token();
    let hash = hash_token(&token);
    let id = repo
        .create(
            ws,
            &hash,
            None,
            WorkspaceRole::Guest,
            Some(owner),
            None,
            None,
        )
        .await
        .expect("create open link");

    let now = OffsetDateTime::now_utc();
    assert!(repo
        .find_active_by_token_hash(&hash, now)
        .await
        .unwrap()
        .is_some());

    // Revoke is idempotent: first call flips it, second is a no-op.
    assert!(repo.revoke(id).await.unwrap(), "first revoke applies");
    assert!(!repo.revoke(id).await.unwrap(), "second revoke is a no-op");

    // A revoked invite no longer resolves, and cannot be incremented.
    assert!(repo
        .find_active_by_token_hash(&hash, now)
        .await
        .unwrap()
        .is_none());
    assert!(
        !repo.increment_use(id).await.unwrap(),
        "revoked invite increments nothing"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_expired_is_not_active() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;

    let token = generate_token();
    let hash = hash_token(&token);
    // Already expired one hour ago.
    let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
    repo.create(
        ws,
        &hash,
        None,
        WorkspaceRole::Member,
        Some(owner),
        None,
        Some(past),
    )
    .await
    .expect("create expired invite");

    let now = OffsetDateTime::now_utc();
    assert!(
        repo.find_active_by_token_hash(&hash, now)
            .await
            .unwrap()
            .is_none(),
        "expired invite is not active"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_list_is_workspace_scoped_and_newest_first() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws_a = new_workspace(&p, owner).await;
    let ws_b = new_workspace(&p, owner).await;

    let id1 = repo
        .create(
            ws_a,
            &hash_token(&generate_token()),
            None,
            WorkspaceRole::Member,
            Some(owner),
            None,
            None,
        )
        .await
        .unwrap();
    let id2 = repo
        .create(
            ws_a,
            &hash_token(&generate_token()),
            None,
            WorkspaceRole::Guest,
            Some(owner),
            None,
            None,
        )
        .await
        .unwrap();
    // A B-scoped invite must never appear in A's listing.
    repo.create(
        ws_b,
        &hash_token(&generate_token()),
        None,
        WorkspaceRole::Member,
        Some(owner),
        None,
        None,
    )
    .await
    .unwrap();

    let a_list = repo.list_for_workspace(ws_a).await.unwrap();
    assert_eq!(a_list.len(), 2, "only A's two invites");
    assert!(
        a_list.iter().all(|i| i.workspace_id == ws_a),
        "strictly A-scoped"
    );
    // Newest first: id2 (created later) precedes id1.
    assert_eq!(a_list[0].id, id2);
    assert_eq!(a_list[1].id, id1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn invitation_governance_rechecks_admin_scope_and_audits_atomically() {
    let p = pool();
    let repo = InvitationRepo::new(p.clone());
    let workspaces = crate::WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let admin = new_participant(&p).await;
    let member = new_participant(&p).await;
    let outsider = new_participant(&p).await;
    let ws = new_workspace(&p, owner).await;
    workspaces
        .add_member(ws, admin, WorkspaceRole::Admin)
        .await
        .unwrap();
    workspaces
        .add_member(ws, member, WorkspaceRole::Member)
        .await
        .unwrap();

    let id = repo
        .create_authorized(
            ws,
            &hash_token(&generate_token()),
            Some("governed@example.com"),
            WorkspaceRole::Member,
            admin,
            Some(2),
            None,
        )
        .await
        .unwrap();
    let create_audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM audit_events
          WHERE workspace_id = $1
            AND actor_id = $2
            AND action = 'invitation.create'
            AND target = $3",
    )
    .bind(ws.to_uuid())
    .bind(admin.to_uuid())
    .bind(id.to_string())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(create_audit, 1);
    assert!(matches!(
        repo.create_authorized(
            ws,
            &hash_token(&generate_token()),
            None,
            WorkspaceRole::Owner,
            admin,
            None,
            None,
        )
        .await,
        Err(Error::Forbidden(_))
    ));
    assert_eq!(
        repo.list_for_workspace_authorized(ws, admin)
            .await
            .unwrap()
            .iter()
            .filter(|invitation| invitation.id == id)
            .count(),
        1
    );
    assert!(matches!(
        repo.revoke_authorized(id, outsider).await,
        Err(Error::NotFound(_))
    ));

    let raw = sqlx::query(
        "INSERT INTO invitations
             (id, workspace_id, token_hash, role, created_by, use_count)
         VALUES ($1, $2, $3, 'member', $4, 0)",
    )
    .bind(InvitationId::new().to_uuid())
    .bind(ws.to_uuid())
    .bind(hash_token(&generate_token()))
    .bind(member.to_uuid())
    .execute(&p)
    .await
    .expect_err("database trigger rejects a non-admin creator");
    assert_eq!(
        raw.as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("invitations_creator_scope_chk")
    );

    let mut demotion = p.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut demotion)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(ws.to_uuid())
    .bind(admin.to_uuid())
    .execute(&mut *demotion)
    .await
    .unwrap();
    let raced_repo = repo.clone();
    let mut raced = tokio::spawn(async move { raced_repo.revoke_authorized(id, admin).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "revoke waits behind a concurrent role change"
    );
    demotion.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
    let revoked_at: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT revoked_at FROM invitations WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert!(revoked_at.is_none());

    assert!(repo.revoke_authorized(id, owner).await.unwrap());
    let revoke_audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM audit_events
          WHERE workspace_id = $1
            AND actor_id = $2
            AND action = 'invitation.revoke'
            AND target = $3",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .bind(id.to_string())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(revoke_audit, 1);
}
