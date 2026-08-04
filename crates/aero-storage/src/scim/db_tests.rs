use super::db_test_support::{new_participant, new_workspace, pool};
use super::*;
use crate::workspace::WorkspaceRepo;
use crate::{DeactivationRepo, RoomRepo};
use aero_common::{RoomKind, WorkspaceRole};

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_token_create_resolve_and_revoke() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let ws_repo = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&ws_repo, owner).await;

    let secret = format!("tok-{}", uuid::Uuid::new_v4());
    let hash = hash_token(&secret);
    let id = scim.create_token(ws, &hash, Some("okta")).await.unwrap();

    // The plaintext's hash resolves to the workspace …
    assert_eq!(
        scim.workspace_for_token_hash(&hash).await.unwrap(),
        Some(ws)
    );
    // … and an unknown hash does not.
    assert_eq!(
        scim.workspace_for_token_hash("deadbeef").await.unwrap(),
        None
    );
    assert_eq!(scim.token_workspace(id).await.unwrap(), Some(ws));

    // Revoking takes it out of active resolution.
    assert!(
        scim.revoke_token(id).await.unwrap(),
        "first revoke succeeds"
    );
    assert_eq!(scim.workspace_for_token_hash(&hash).await.unwrap(), None);
    assert!(
        !scim.revoke_token(id).await.unwrap(),
        "second revoke is a no-op"
    );

    // Request-facing management uses a transaction-owned effective-admin
    // decision and exposes only safe metadata in the inventory.
    let managed_secret = format!("managed-{}", uuid::Uuid::new_v4());
    let managed = scim
        .create_token_authorized(ws, &hash_token(&managed_secret), Some("managed"), owner)
        .await
        .unwrap();
    let inventory = scim.list_tokens(ws).await.unwrap();
    let managed_row = inventory
        .iter()
        .find(|token| token.id == managed)
        .expect("new managed credential is discoverable");
    assert_eq!(managed_row.label.as_deref(), Some("managed"));
    assert!(managed_row.revoked_at.is_none());
    assert!(
        scim.revoke_token_authorized(managed, owner).await.unwrap(),
        "effective owner revokes the credential"
    );
    let revoked = scim
        .list_tokens(ws)
        .await
        .unwrap()
        .into_iter()
        .find(|token| token.id == managed)
        .expect("revoked credentials remain auditable");
    assert!(revoked.revoked_at.is_some());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_user_create_list_find_and_deactivate() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let ws_repo = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&ws_repo, owner).await;

    let user_name = format!("alice-{}@example.com", ParticipantId::new());
    let created = scim
        .provision_user(ws, "Alice SCIM", &user_name, Some("ext-123"), true)
        .await
        .unwrap();
    let participant = created.participant_id;
    assert!(created.active);
    let deactivations = DeactivationRepo::new(p.clone());

    // get + find_by_user_name round-trip.
    assert_eq!(
        scim.get_user(ws, participant)
            .await
            .unwrap()
            .map(|u| u.user_name.clone()),
        Some(user_name.clone())
    );
    let found = scim
        .find_by_user_name(ws, &user_name)
        .await
        .unwrap()
        .expect("found");
    assert_eq!(found.participant_id, participant);
    assert_eq!(found.external_id.as_deref(), Some("ext-123"));

    // list with the userName filter returns exactly this one, total reflects filter.
    let (rows, total) = scim
        .list_users(ws, Some(&user_name), Some(1), Some(50))
        .await
        .unwrap();
    assert_eq!(total, 1, "exactly one matches the filter");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].participant_id, participant);

    // Deactivate (PATCH active=false): the SCIM row is RETAINED but inactive,
    // so a subsequent GET still returns it (RFC: an inactive user is valid).
    let deactivated = scim
        .set_active(ws, participant, false)
        .await
        .unwrap()
        .expect("exists");
    assert!(!deactivated.active, "marked inactive");
    assert!(
        deactivations.is_deactivated(ws, participant).await.unwrap(),
        "inactive SCIM user is fenced from effective workspace access"
    );
    assert!(
        ws_repo.is_member(ws, participant).await.unwrap(),
        "suspension retains membership topology"
    );
    assert!(
        scim.get_user(ws, participant).await.unwrap().is_some(),
        "deactivated row retained for later GET/reactivation"
    );
    // …and can be reactivated.
    let reactivated = scim
        .set_active(ws, participant, true)
        .await
        .unwrap()
        .expect("exists");
    assert!(reactivated.active);
    assert!(!deactivations.is_deactivated(ws, participant).await.unwrap());

    // Delete (DELETE): the SCIM row is REMOVED (RFC 7644 §3.6 — a later GET
    // 404s) + membership revoked, but the global participant is retained.
    assert!(
        scim.delete_user(ws, participant).await.unwrap(),
        "row existed"
    );
    assert!(
        scim.get_user(ws, participant).await.unwrap().is_none(),
        "deleted SCIM row is gone (GET would 404)"
    );
    assert!(
        !ws_repo.is_member(ws, participant).await.unwrap(),
        "membership removed"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_duplicate_user_name_conflicts() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let ws_repo = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&ws_repo, owner).await;

    let name = format!("dup-{}@example.com", uuid::Uuid::new_v4());
    scim.provision_user(ws, "duplicate-a", &name, None, true)
        .await
        .unwrap();
    // A second user with the same userName in the same workspace must fail
    // (the unique index), which the route maps to 409 Conflict.
    assert!(
        scim.provision_user(ws, "duplicate-b", &name, None, true)
            .await
            .is_err(),
        "duplicate userName within a workspace is rejected"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_and_oidc_share_one_canonical_external_identity() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let sso = crate::SsoRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&workspaces, owner).await;
    let marker = uuid::Uuid::new_v4();
    let issuer = format!("https://snaplink.example/{marker}");
    let subject = format!("snaplink-user-{marker}");
    let user_name = format!("canonical-{marker}@example.com");

    let provisioned = scim
        .provision_user_with_identity(
            ws,
            "SCIM canonical",
            &user_name,
            Some(&subject),
            true,
            Some(&issuer),
        )
        .await
        .expect("SCIM pre-provisioning");
    let logged_in = sso
        .resolve_or_provision_human(
            &issuer,
            &subject,
            "OIDC duplicate must not be created",
            Some(&user_name),
            ws,
        )
        .await
        .expect("OIDC resolves pre-provisioned identity");

    assert_eq!(logged_in, provisioned.participant_id);
    let rebind = scim
        .update_user_atomic(
            ws,
            provisioned.participant_id,
            None,
            Some(Some("different-snaplink-subject")),
            None,
            None,
        )
        .await
        .expect_err("routine SCIM PATCH cannot rebind a login identity");
    assert!(matches!(
        rebind,
        ScimUserWriteError::IdentitySubjectImmutable
    ));
    let identity_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sso_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&issuer)
    .bind(&subject)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(identity_count, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_inactive_provision_rejects_existing_workspace_owner_atomically() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let sso = crate::SsoRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let deactivations = DeactivationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let original_display_name = format!("scim-user-{owner}");
    let ws = new_workspace(&workspaces, owner).await;
    let marker = uuid::Uuid::new_v4();
    let issuer = format!("https://snaplink.example/{marker}");
    let subject = format!("existing-owner-{marker}");
    let original_email = format!("owner-{marker}@example.com");
    sso.link(&issuer, &subject, owner, Some(&original_email))
        .await
        .expect("bind the existing workspace owner");

    let error = scim
        .provision_user_with_identity(
            ws,
            "must roll back",
            &format!("owner-scim-{marker}@example.com"),
            Some(&subject),
            false,
            Some(&issuer),
        )
        .await
        .expect_err("inactive SCIM provisioning cannot deactivate an owner");
    assert!(matches!(error, ScimUserWriteError::OwnerDeprovision));

    assert!(scim.get_user(ws, owner).await.unwrap().is_none());
    assert!(!deactivations.is_deactivated(ws, owner).await.unwrap());
    assert_eq!(
        workspaces.member_role(ws, owner).await.unwrap(),
        Some(WorkspaceRole::Owner)
    );
    let (display_name, email): (String, Option<String>) = sqlx::query_as(
        r"SELECT participant.display_name, identity.email
                FROM participants participant
                JOIN sso_identities identity
                  ON identity.participant_id = participant.id
                 AND identity.issuer = $2
                 AND identity.subject = $3
               WHERE participant.id = $1",
    )
    .bind(owner.to_uuid())
    .bind(&issuer)
    .bind(&subject)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(display_name, original_display_name);
    assert_eq!(email.as_deref(), Some(original_email.as_str()));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let sso = crate::SsoRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let deactivations = DeactivationRepo::new(p.clone());
    let default_workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
    let has_effective_member: bool =
        sqlx::query_scalar("SELECT aero_workspace_has_effective_member($1)")
            .bind(default_workspace.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert!(
        !has_effective_member,
        "this fresh-database regression requires the dormant nil workspace"
    );

    let participant = new_participant(&p).await;
    let marker = uuid::Uuid::new_v4();
    let issuer = format!("https://snaplink.example/{marker}");
    let subject = format!("first-nil-owner-{marker}");
    sso.link(&issuer, &subject, participant, None)
        .await
        .expect("bind the candidate first member");

    let error = scim
        .provision_user_with_identity(
            default_workspace,
            "must roll back nil bootstrap",
            &format!("first-nil-owner-{marker}@example.com"),
            Some(&subject),
            false,
            Some(&issuer),
        )
        .await
        .expect_err("the first nil-workspace member is promoted before suspension");
    assert!(matches!(error, ScimUserWriteError::OwnerDeprovision));

    assert!(scim
        .get_user(default_workspace, participant)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        workspaces
            .member_role(default_workspace, participant)
            .await
            .unwrap(),
        None,
        "bootstrap membership is rolled back with the SCIM aggregate"
    );
    assert!(!deactivations
        .is_deactivated(default_workspace, participant)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_active_provision_clears_existing_workspace_deactivation() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let sso = crate::SsoRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let deactivations = DeactivationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&workspaces, owner).await;
    let participant = new_participant(&p).await;
    workspaces
        .add_member(ws, participant, WorkspaceRole::Member)
        .await
        .expect("enroll existing identity");
    deactivations
        .deactivate(ws, participant, owner)
        .await
        .expect("install the pre-existing access fence");

    let marker = uuid::Uuid::new_v4();
    let issuer = format!("https://snaplink.example/{marker}");
    let subject = format!("existing-deactivated-{marker}");
    sso.link(&issuer, &subject, participant, None)
        .await
        .expect("bind the existing deactivated identity");

    let provisioned = scim
        .provision_user_with_identity(
            ws,
            "SCIM reactivated identity",
            &format!("reactivated-{marker}@example.com"),
            Some(&subject),
            true,
            Some(&issuer),
        )
        .await
        .expect("active SCIM provisioning removes the access fence");
    assert_eq!(provisioned.participant_id, participant);
    assert!(provisioned.active);
    assert!(workspaces.is_member(ws, participant).await.unwrap());
    assert!(!deactivations.is_deactivated(ws, participant).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_cannot_recreate_an_erased_external_identity() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&workspaces, owner).await;
    let marker = uuid::Uuid::new_v4();
    let issuer = format!("https://snaplink.example/{marker}");
    let subject = format!("erased-user-{marker}");
    let display_name = format!("must-not-exist-{marker}");

    sqlx::query(
        r"INSERT INTO sso_identity_tombstones
                  (issuer, subject, former_participant_id, reason)
               VALUES ($1, $2, $3, 'test_erasure')",
    )
    .bind(&issuer)
    .bind(&subject)
    .bind(ParticipantId::new().to_uuid())
    .execute(&p)
    .await
    .unwrap();

    let error = scim
        .provision_user_with_identity(
            ws,
            &display_name,
            &format!("erased-{marker}@example.com"),
            Some(&subject),
            true,
            Some(&issuer),
        )
        .await
        .expect_err("ordinary SCIM retries must respect erasure tombstones");
    assert!(matches!(error, ScimUserWriteError::IdentityTombstoned));
    let participant_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM participants WHERE display_name = $1")
            .bind(&display_name)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(participant_count, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_user_aggregate_mutations_are_atomic_and_tenant_scoped() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let ws_repo = WorkspaceRepo::new(p.clone());
    let rooms = RoomRepo::new(p.clone());
    let deactivations = DeactivationRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&ws_repo, owner).await;
    let room = rooms
        .create_in_workspace(ws, RoomKind::Channel, Some("SCIM room".into()), owner)
        .await
        .unwrap();

    let inactive_name = format!("inactive-{}@example.com", uuid::Uuid::new_v4());
    let inactive = scim
        .provision_user(ws, "Inactive SCIM", &inactive_name, None, false)
        .await
        .unwrap();
    assert!(ws_repo
        .is_member(ws, inactive.participant_id)
        .await
        .unwrap());
    assert!(deactivations
        .is_deactivated(ws, inactive.participant_id)
        .await
        .unwrap());

    scim.update_user_atomic(ws, inactive.participant_id, None, None, Some(true), None)
        .await
        .unwrap()
        .expect("reactivated");
    assert!(ws_repo
        .is_member(ws, inactive.participant_id)
        .await
        .unwrap());
    assert!(!deactivations
        .is_deactivated(ws, inactive.participant_id)
        .await
        .unwrap());
    rooms
        .add_member(room.id, inactive.participant_id)
        .await
        .unwrap();

    let colliding_name = format!("collision-{}@example.com", uuid::Uuid::new_v4());
    let other = scim
        .provision_user(ws, "Collision owner", &colliding_name, None, true)
        .await
        .unwrap();
    let before_name: String =
        sqlx::query_scalar("SELECT display_name FROM participants WHERE id = $1")
            .bind(inactive.participant_id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();

    let collision = scim
        .update_user_atomic(
            ws,
            inactive.participant_id,
            Some(&other.user_name),
            None,
            Some(false),
            Some("must roll back"),
        )
        .await;
    assert!(collision.is_err(), "duplicate userName must fail");
    let after = scim
        .get_user(ws, inactive.participant_id)
        .await
        .unwrap()
        .expect("mapping retained");
    assert_eq!(after.user_name, inactive_name);
    assert!(after.active, "active flag rolled back");
    let after_name: String =
        sqlx::query_scalar("SELECT display_name FROM participants WHERE id = $1")
            .bind(inactive.participant_id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(after_name, before_name, "profile update rolled back");
    assert!(ws_repo
        .is_member(ws, inactive.participant_id)
        .await
        .unwrap());
    assert!(rooms
        .is_member(room.id, inactive.participant_id)
        .await
        .unwrap());

    scim.update_user_atomic(ws, inactive.participant_id, None, None, Some(false), None)
        .await
        .unwrap()
        .expect("deactivated");
    assert!(ws_repo
        .is_member(ws, inactive.participant_id)
        .await
        .unwrap());
    assert!(
        rooms
            .is_member(room.id, inactive.participant_id)
            .await
            .unwrap(),
        "suspension retains private-room topology"
    );
    assert!(deactivations
        .is_deactivated(ws, inactive.participant_id)
        .await
        .unwrap());
    scim.update_user_atomic(ws, inactive.participant_id, None, None, Some(true), None)
        .await
        .unwrap()
        .expect("reactivated");
    assert!(ws_repo
        .is_member(ws, inactive.participant_id)
        .await
        .unwrap());
    assert!(
        rooms
            .is_member(room.id, inactive.participant_id)
            .await
            .unwrap(),
        "reactivation restores access to retained room topology"
    );
    assert!(!deactivations
        .is_deactivated(ws, inactive.participant_id)
        .await
        .unwrap());

    let ordinary = new_participant(&p).await;
    ws_repo
        .add_member(ws, ordinary, WorkspaceRole::Member)
        .await
        .unwrap();
    rooms.add_member(room.id, ordinary).await.unwrap();
    let ordinary_name: String =
        sqlx::query_scalar("SELECT display_name FROM participants WHERE id = $1")
            .bind(ordinary.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert!(
        scim.update_user_atomic(
            ws,
            ordinary,
            Some("forbidden@example.com"),
            None,
            Some(false),
            Some("cross-tenant overwrite"),
        )
        .await
        .unwrap()
        .is_none(),
        "non-SCIM ids are not mutable through SCIM"
    );
    assert!(
        !scim.delete_user(ws, ordinary).await.unwrap(),
        "DELETE of a non-SCIM member is a scoped no-op"
    );
    let ordinary_after: String =
        sqlx::query_scalar("SELECT display_name FROM participants WHERE id = $1")
            .bind(ordinary.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(ordinary_after, ordinary_name);
    assert!(ws_repo.is_member(ws, ordinary).await.unwrap());
    assert!(rooms.is_member(room.id, ordinary).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_scim_username_conflict_leaves_no_ghost_participant() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let ws_repo = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&ws_repo, owner).await;
    let user_name = format!("race-{}@example.com", uuid::Uuid::new_v4());
    let display_a = format!("scim-race-a-{}", uuid::Uuid::new_v4());
    let display_b = format!("scim-race-b-{}", uuid::Uuid::new_v4());

    let (a, b) = tokio::join!(
        scim.provision_user(ws, &display_a, &user_name, None, true),
        scim.provision_user(ws, &display_b, &user_name, None, true)
    );
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "the unique userName admits exactly one transaction"
    );
    let surviving_id = a
        .ok()
        .or_else(|| b.ok())
        .expect("one provision succeeds")
        .participant_id;
    let participant_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM participants WHERE display_name = $1 OR display_name = $2",
    )
    .bind(&display_a)
    .bind(&display_b)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        participant_count, 1,
        "losing transaction leaves no participant"
    );
    assert!(ws_repo.is_member(ws, surviving_id).await.unwrap());
    let member_count: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
               FROM workspace_members wm
               JOIN participants p ON p.id = wm.participant_id
               WHERE wm.workspace_id = $1
                 AND (p.display_name = $2 OR p.display_name = $3)",
    )
    .bind(ws.to_uuid())
    .bind(&display_a)
    .bind(&display_b)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(member_count, 1, "losing transaction leaves no membership");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_cannot_deprovision_a_workspace_owner() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let original_owner = new_participant(&p).await;
    let ws = new_workspace(&workspaces, original_owner).await;

    let promoted = scim
        .provision_user(
            ws,
            "Promoted SCIM owner",
            &format!("owner-{}@example.com", uuid::Uuid::new_v4()),
            None,
            true,
        )
        .await
        .unwrap();
    workspaces
        .update_member_role(ws, promoted.participant_id, WorkspaceRole::Owner)
        .await
        .unwrap();

    assert!(matches!(
        scim.set_active(ws, promoted.participant_id, false)
            .await
            .expect_err("SCIM deactivate must not strand owner governance"),
        ScimUserWriteError::OwnerDeprovision
    ));
    assert!(matches!(
        scim.delete_user(ws, promoted.participant_id)
            .await
            .expect_err("SCIM delete must not strand owner governance"),
        ScimUserWriteError::OwnerDeprovision
    ));
    assert!(workspaces
        .is_member(ws, promoted.participant_id)
        .await
        .unwrap());
    assert!(
        scim.get_user(ws, promoted.participant_id)
            .await
            .unwrap()
            .expect("mapping is retained")
            .active
    );

    // An explicit ownership transfer/demotion makes normal deprovisioning
    // legal again.
    workspaces
        .update_member_role(ws, promoted.participant_id, WorkspaceRole::Member)
        .await
        .unwrap();
    assert!(scim.delete_user(ws, promoted.participant_id).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_role_promotion_and_scim_delete_cannot_resurrect_membership() {
    let p = pool();
    let scim = ScimRepo::new(p.clone());
    let workspaces = WorkspaceRepo::new(p.clone());
    let owner = new_participant(&p).await;
    let ws = new_workspace(&workspaces, owner).await;
    let user = scim
        .provision_user(
            ws,
            "SCIM role race",
            &format!("role-race-{}@example.com", uuid::Uuid::new_v4()),
            None,
            true,
        )
        .await
        .unwrap();

    let (promoted, deleted) = tokio::join!(
        workspaces.change_member_role_authorized(
            ws,
            owner,
            user.participant_id,
            WorkspaceRole::Owner,
        ),
        scim.delete_user(ws, user.participant_id),
    );
    let mapping = scim.get_user(ws, user.participant_id).await.unwrap();
    let role = workspaces
        .member_role(ws, user.participant_id)
        .await
        .unwrap();
    match (mapping, role) {
        (Some(mapping), Some(WorkspaceRole::Owner)) => {
            assert!(mapping.active);
            assert!(promoted.is_ok());
            assert!(matches!(deleted, Err(ScimUserWriteError::OwnerDeprovision)));
        }
        (None, None) => {
            assert!(matches!(
                promoted,
                Err(crate::WorkspaceMemberWriteError::MemberNotFound)
            ));
            assert!(matches!(deleted, Ok(true)));
        }
        state => panic!("SCIM mapping and workspace membership diverged: {state:?}"),
    }
}
