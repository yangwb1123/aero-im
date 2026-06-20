//! Workspace (tenant / org) + membership repository.
//!
//! Backs `migrations/0006_workspaces.sql`. A workspace groups members (each with
//! a [`WorkspaceRole`]) and channels (`rooms.workspace_id`). `participants` stay
//! global identities; tenancy is a membership edge, not a property of the user.
//!
//! This repo is purely additive: it introduces a NEW [`WorkspaceRepo`] and does
//! not touch existing repos. Threading `workspace_id` into existing room/message
//! queries is a later batch.
//!
//! Authorization predicates ([`role_can_invite`], [`role_can_remove`], …) are
//! free functions with no DB dependency so they unit-test directly, mirroring how
//! the rest of `aero-storage` keeps testable logic separate from live SQL.

#[allow(unused_imports)]
use aero_common::{MessageId, ParticipantId, RoomId, Workspace, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;


#[derive(Clone)]
pub struct WorkspaceRepo {
    pub(crate) pool: PgPool,
}

impl WorkspaceRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a workspace and enroll its creator as `owner`, atomically.
    pub async fn create(
        &self,
        name: String,
        slug: String,
        created_by: ParticipantId,
    ) -> Result<Workspace, sqlx::Error> {
        let id = WorkspaceId::new();
        let created_at = time::OffsetDateTime::now_utc();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"INSERT INTO workspaces (id, name, slug, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(&name)
        .bind(&slug)
        .bind(created_by.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(created_by.to_uuid())
        .bind(WorkspaceRole::Owner.as_str())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(Workspace {
            id,
            name,
            slug,
            created_by: Some(created_by),
            created_at,
            logo_url: None,
            color_scheme: None,
            custom_domain: None,
            description: None,
        })
    }

    /// Fetch a single workspace row, or `None` if it does not exist. Used by the
    /// export path to resolve the tenant before gathering its data.
    pub async fn get(&self, workspace: WorkspaceId) -> Result<Option<Workspace>, sqlx::Error> {
        let row = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime,
             Option<String>, Option<String>, Option<String>, Option<String>),
        >(
            r"SELECT id, name, slug, created_by, created_at,
                     logo_url, color_scheme, custom_domain, description
               FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, name, slug, by, at, logo_url, color_scheme, custom_domain, description)| Workspace {
            id: WorkspaceId::from_uuid(id),
            name,
            slug,
            created_by: by.map(ParticipantId::from_uuid),
            created_at: at,
            logo_url,
            color_scheme,
            custom_domain,
            description,
        }))
    }

}

// ---------- Pure authorization predicates (DB-free, unit-tested) ----------

/// Can a member with `role` invite new members? Admins and owners may.
#[must_use]
pub fn role_can_invite(role: WorkspaceRole) -> bool {
    role.can_administer()
}

/// Can a member with `role` remove other members? Admins and owners may.
#[must_use]
pub fn role_can_remove(role: WorkspaceRole) -> bool {
    role.can_administer()
}

/// Can an actor holding `actor` assign the role `target` to someone?
///
/// Rules:
/// - Only administrators (admin/owner) may assign roles at all.
/// - You may never grant a role strictly above your own (no privilege
///   escalation): an admin can mint members/guests/admins but not owners.
#[must_use]
pub fn role_can_assign(actor: WorkspaceRole, target: WorkspaceRole) -> bool {
    actor.can_administer() && actor.at_least(target)
}

/// Can `actor` change/remove the membership of a member currently holding
/// `subject`? Administrators may act on anyone at or below their own privilege;
/// nobody may act on someone strictly more privileged than themselves.
#[must_use]
pub fn role_can_manage_member(actor: WorkspaceRole, subject: WorkspaceRole) -> bool {
    actor.can_administer() && actor.at_least(subject)
}

// ---------- Pure retention-policy helpers (DB-free, unit-tested) ----------

/// Smallest accepted retention window, in days. A policy must keep messages for
/// at least one day; anything shorter is treated as a configuration error rather
/// than "soft-delete almost everything immediately".
pub const MIN_RETENTION_DAYS: i32 = 1;

/// Validate a retention setting before it is persisted.
///
/// `None` (keep forever) is always valid. A `Some(n)` is valid only when
/// `n >= MIN_RETENTION_DAYS`; `Err` carries the offending value so the route
/// layer can surface a `400`. Kept DB-free so the rule is unit-tested offline.
///
/// # Errors
/// Returns `Err(days)` when `days = Some(n)` with `n < MIN_RETENTION_DAYS`.
pub fn validate_retention_days(days: Option<i32>) -> Result<(), i32> {
    match days {
        None => Ok(()),
        Some(n) if n >= MIN_RETENTION_DAYS => Ok(()),
        Some(n) => Err(n),
    }
}

/// Compute the soft-delete cutoff for a `days`-day retention window relative to
/// `now`: messages strictly older than the returned instant are expired.
///
/// This mirrors, in Rust, the `now - make_interval(days => …)` the sweep does in
/// SQL — exposed as a pure function so the cutoff arithmetic is unit-testable
/// without a database. (`days` is the workspace's configured window.)
#[must_use]
pub fn retention_cutoff(now: time::OffsetDateTime, days: i32) -> time::OffsetDateTime {
    now - time::Duration::days(i64::from(days))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    #[test]
    fn invite_and_remove_require_admin() {
        assert!(role_can_invite(WorkspaceRole::Owner));
        assert!(role_can_invite(WorkspaceRole::Admin));
        assert!(!role_can_invite(WorkspaceRole::Member));
        assert!(!role_can_invite(WorkspaceRole::Guest));

        // remove mirrors invite
        for r in ALL {
            assert_eq!(role_can_remove(r), role_can_invite(r), "role {r:?}");
        }
    }

    #[test]
    fn non_admins_can_never_assign() {
        for target in ALL {
            assert!(!role_can_assign(WorkspaceRole::Member, target));
            assert!(!role_can_assign(WorkspaceRole::Guest, target));
        }
    }

    #[test]
    fn admin_cannot_grant_owner_but_can_grant_lower() {
        assert!(!role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Owner));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Admin));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Member));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Guest));
    }

    #[test]
    fn owner_can_grant_anything() {
        for target in ALL {
            assert!(role_can_assign(WorkspaceRole::Owner, target), "target {target:?}");
        }
    }

    #[test]
    fn no_privilege_escalation_via_assign() {
        // For every actor, granting a role strictly above the actor must fail.
        for actor in ALL {
            for target in ALL {
                if target.rank() > actor.rank() {
                    assert!(
                        !role_can_assign(actor, target),
                        "actor {actor:?} must not grant higher {target:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn manage_member_respects_hierarchy() {
        // Admin can manage members/guests/other admins, but not owners.
        assert!(role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Member));
        assert!(role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Admin));
        assert!(!role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Owner));

        // Owner can manage anyone.
        for subject in ALL {
            assert!(role_can_manage_member(WorkspaceRole::Owner, subject), "subject {subject:?}");
        }

        // Regular members and guests can manage nobody.
        for subject in ALL {
            assert!(!role_can_manage_member(WorkspaceRole::Member, subject));
            assert!(!role_can_manage_member(WorkspaceRole::Guest, subject));
        }
    }

    // ----- retention-policy helpers -----

    #[test]
    fn validate_retention_accepts_none_and_positive() {
        // None = keep forever, always valid.
        assert!(validate_retention_days(None).is_ok());
        // The floor and anything above it are accepted.
        assert!(validate_retention_days(Some(MIN_RETENTION_DAYS)).is_ok());
        assert!(validate_retention_days(Some(1)).is_ok());
        assert!(validate_retention_days(Some(30)).is_ok());
        assert!(validate_retention_days(Some(3650)).is_ok());
        assert!(validate_retention_days(Some(i32::MAX)).is_ok());
    }

    #[test]
    fn validate_retention_rejects_below_floor_and_echoes_value() {
        // Zero and negatives are configuration errors; the bad value is returned.
        assert_eq!(validate_retention_days(Some(0)), Err(0));
        assert_eq!(validate_retention_days(Some(-1)), Err(-1));
        assert_eq!(validate_retention_days(Some(-365)), Err(-365));
        assert_eq!(validate_retention_days(Some(i32::MIN)), Err(i32::MIN));
    }

    #[test]
    fn retention_cutoff_subtracts_exactly_n_days() {
        // A fixed reference instant; the cutoff is `now - days`.
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(retention_cutoff(now, 1), now - time::Duration::days(1));
        assert_eq!(retention_cutoff(now, 30), now - time::Duration::days(30));
        // A message exactly `days` old sits ON the cutoff (the sweep uses a
        // strict `<`, so it is NOT yet expired); one a hair older is expired.
        let cutoff = retention_cutoff(now, 7);
        let exactly_seven_days_old = now - time::Duration::days(7);
        assert_eq!(exactly_seven_days_old, cutoff, "boundary is inclusive of cutoff");
        assert!(now - time::Duration::days(8) < cutoff, "8-day-old is past cutoff");
        assert!(now - time::Duration::days(6) > cutoff, "6-day-old is within window");
    }
}

/// PG-gated integration tests for the compliance export + delete path. Run with
/// a live Postgres + applied migrations:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored workspace_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in
/// CI); the orchestrator runs them against a live database.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::audit::AuditRepo;

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
            .bind(format!("ws-export-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a workspace owned by `owner`, a channel in it, and one message in
    /// that channel. Returns `(workspace, room, message)` ids.
    async fn seed_workspace(
        repo: &WorkspaceRepo,
        p: &PgPool,
        owner: ParticipantId,
    ) -> (WorkspaceId, RoomId, MessageId) {
        let ws = repo
            .create("Export WS".into(), format!("exp-{}", WorkspaceId::new()), owner)
            .await
            .expect("create workspace");

        let room = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'channel', $2, $3, now(), $4)",
        )
        .bind(room.to_uuid())
        .bind("general")
        .bind(owner.to_uuid())
        .bind(ws.id.to_uuid())
        .execute(p)
        .await
        .expect("insert room");

        let msg = MessageId::new();
        sqlx::query(
            r"INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
               VALUES ($1, $2, $3, $4, $5, now())",
        )
        .bind(msg.to_uuid())
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "text": "hello tenant" }]))
        .bind("hello tenant")
        .execute(p)
        .await
        .expect("insert message");

        (ws.id, room, msg)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_export_returns_members_rooms_and_messages() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, msg) = seed_workspace(&repo, &p, owner).await;

        let export = repo.export(ws).await.unwrap().expect("workspace exists");

        // Workspace identity.
        assert_eq!(export.workspace.id, ws);
        // Owner is enrolled as a member by `create`.
        assert!(
            export.members.iter().any(|m| m.participant_id == owner && m.role == WorkspaceRole::Owner),
            "owner must appear as a member"
        );
        // The seeded channel + its message are present.
        let r = export.rooms.iter().find(|r| r.room.id == room).expect("room exported");
        assert!(!r.message_cap_hit, "tiny room is not capped");
        assert!(r.messages.iter().any(|m| m.id == msg), "message exported");
        // The workspace.create audit event is captured.
        assert!(
            export.audit_events.iter().all(|e| e.workspace_id == ws),
            "audit events are tenant-scoped"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_export_missing_is_none() {
        let p = pool();
        let repo = WorkspaceRepo::new(p);
        assert!(repo.export(WorkspaceId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_removes_workspace_rooms_members_and_audit() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, msg) = seed_workspace(&repo, &p, owner).await;
        // A room-scoped row in a NON-FK table (message_drafts) — proves the explicit
        // cleanup catches tables `DELETE FROM rooms` would otherwise orphan.
        sqlx::query("INSERT INTO message_drafts (participant_id, room_id, blocks) VALUES ($1,$2,'[]'::jsonb)")
            .bind(owner.to_uuid())
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        // A WORKSPACE-scoped row in a NON-FK table (keyword_alerts).
        sqlx::query(
            "INSERT INTO keyword_alerts (id, participant_id, workspace_id, keyword) VALUES ($1,$2,$3,'secret')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(owner.to_uuid())
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        // A workspace-scoped analytics row (search_click_events, mig 0133 — PII, no
        // FK to workspaces). Its cleanup was added by 51759c2 and silently re-dropped
        // by the workspace.rs→workspace/ split; seed + assert it to guard the full
        // cleanup list, not just message_drafts/keyword_alerts (which passed even
        // against the gutted version).
        sqlx::query(
            "INSERT INTO search_click_events (participant_id, workspace_id, query_text, result_id, result_rank) VALUES ($1,$2,'q',$3,1)",
        )
        .bind(owner.to_uuid())
        .bind(ws.to_uuid())
        .bind(uuid::Uuid::new_v4())
        .execute(&p)
        .await
        .unwrap();
        // An audit row to prove the cascade reaches audit_events.
        AuditRepo::new(p.clone())
            .append(ws, Some(owner), "workspace.delete", None, serde_json::json!({}))
            .await
            .unwrap();

        let deleted = repo.delete(ws).await.unwrap();
        assert!(deleted, "delete reports a row was removed");

        // Workspace gone.
        assert!(repo.get(ws).await.unwrap().is_none(), "workspace row deleted");
        // Members cascade-deleted.
        assert!(!repo.is_member(ws, owner).await.unwrap(), "members cascade-deleted");
        // Room explicitly deleted (rooms→workspaces FK has no cascade).
        let room_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(room_left, 0, "room deleted with its workspace");
        // Messages cascade-deleted with the room.
        let msg_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
                .bind(msg.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(msg_left, 0, "messages cascade-deleted with the room");
        // Audit rows cascade-deleted with the workspace.
        let audit_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
                .bind(ws.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(audit_left, 0, "audit events cascade-deleted with the workspace");
        // Non-FK room-scoped content (drafts) is explicitly cleaned, not orphaned.
        let draft_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_drafts WHERE room_id = $1")
                .bind(room.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(draft_left, 0, "non-FK room-scoped drafts cleaned, not orphaned");
        // Non-FK workspace-scoped content (keyword alerts) is cleaned too.
        let kw_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM keyword_alerts WHERE workspace_id = $1")
                .bind(ws.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(kw_left, 0, "non-FK workspace-scoped keyword alerts cleaned, not orphaned");
        // Analytics PII (search_click_events) cleaned — the cleanup re-dropped by the
        // split. Guards GDPR erasure completeness for the broader non-FK table set.
        let clicks_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM search_click_events WHERE workspace_id = $1")
                .bind(ws.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(clicks_left, 0, "non-FK workspace-scoped search_click_events (PII) cleaned, not orphaned");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_missing_reports_false() {
        let p = pool();
        let repo = WorkspaceRepo::new(p);
        assert!(!repo.delete(WorkspaceId::new()).await.unwrap(), "no row to delete");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_does_not_touch_a_second_tenant() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner_a = new_participant(&p).await;
        let owner_b = new_participant(&p).await;
        let (ws_a, room_a, msg_a) = seed_workspace(&repo, &p, owner_a).await;
        let (ws_b, room_b, msg_b) = seed_workspace(&repo, &p, owner_b).await;

        repo.delete(ws_a).await.unwrap();

        // Tenant A is gone …
        assert!(repo.get(ws_a).await.unwrap().is_none());
        // … but tenant B is completely intact: workspace, member, room, message.
        assert!(repo.get(ws_b).await.unwrap().is_some(), "second workspace survives");
        assert!(repo.is_member(ws_b, owner_b).await.unwrap(), "B's member survives");
        let b_export = repo.export(ws_b).await.unwrap().expect("B still exportable");
        assert!(
            b_export.rooms.iter().any(|r| r.room.id == room_b
                && r.messages.iter().any(|m| m.id == msg_b)),
            "B's room + message survive A's deletion"
        );
        // And A's room/message are truly gone (tenant isolation, both directions).
        let a_rooms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = $1")
            .bind(room_a.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        let a_msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
            .bind(msg_a.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!((a_rooms, a_msgs), (0, 0), "A's room + message deleted");
    }

    // ----- retention sweep -----

    /// Insert a message into `room` with an explicit `created_at`, returning its
    /// id. Lets the sweep tests place messages on either side of a retention
    /// cutoff deterministically.
    async fn insert_message_at(
        p: &PgPool,
        room: RoomId,
        sender: ParticipantId,
        created_at: time::OffsetDateTime,
    ) -> MessageId {
        let id = MessageId::new();
        sqlx::query(
            r"INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "text": "retained body" }]))
        .bind("retained body")
        .bind(created_at)
        .execute(p)
        .await
        .expect("insert dated message");
        id
    }

    /// The `(deleted_at, blocks, searchable_text)` of a message — enough to
    /// assert the sweep applied exactly the soft-delete shape.
    async fn message_state(
        p: &PgPool,
        id: MessageId,
    ) -> (Option<time::OffsetDateTime>, serde_json::Value, String) {
        sqlx::query_as::<_, (Option<time::OffsetDateTime>, serde_json::Value, String)>(
            r"SELECT deleted_at, blocks, searchable_text FROM messages WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_one(p)
        .await
        .expect("message exists")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sweep_soft_deletes_only_messages_past_the_window() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        // seed_workspace also inserts one `now()`-dated message we expect to keep.
        let (ws, room, recent_seeded) = seed_workspace(&repo, &p, owner).await;
        repo.set_retention(ws, Some(30)).await.unwrap();

        let now = time::OffsetDateTime::now_utc();
        // 40 days old: strictly past the 30-day cutoff → swept.
        let old = insert_message_at(&p, room, owner, now - time::Duration::days(40)).await;
        // 10 days old: within the window → kept.
        let fresh = insert_message_at(&p, room, owner, now - time::Duration::days(10)).await;

        let swept = repo.sweep_expired_messages(now, Some(ws)).await.unwrap();
        assert_eq!(swept.len(), 1, "exactly the one 40-day-old message is swept");

        // The old message now wears the canonical soft-delete shape.
        let (deleted_at, blocks, text) = message_state(&p, old).await;
        assert!(deleted_at.is_some(), "old message is soft-deleted");
        assert_eq!(blocks, serde_json::json!([]), "blocks cleared to []");
        assert_eq!(text, "", "searchable_text cleared");

        // The within-window messages are untouched.
        assert!(message_state(&p, fresh).await.0.is_none(), "10-day message kept");
        assert!(message_state(&p, recent_seeded).await.0.is_none(), "now() message kept");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sweep_skips_workspaces_without_a_policy() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, _seeded) = seed_workspace(&repo, &p, owner).await;
        // No set_retention call → retention_days stays NULL (keep forever).

        let now = time::OffsetDateTime::now_utc();
        let ancient = insert_message_at(&p, room, owner, now - time::Duration::days(3650)).await;

        let swept = repo.sweep_expired_messages(now, Some(ws)).await.unwrap();
        assert_eq!(swept.len(), 0, "a policy-less workspace is never swept");
        assert!(message_state(&p, ancient).await.0.is_none(), "10-year message kept");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sweep_isolates_workspaces_and_is_idempotent() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner_a = new_participant(&p).await;
        let owner_b = new_participant(&p).await;
        let (ws_a, room_a, _sa) = seed_workspace(&repo, &p, owner_a).await;
        let (ws_b, room_b, _sb) = seed_workspace(&repo, &p, owner_b).await;
        // Only A opts into retention; B keeps forever.
        repo.set_retention(ws_a, Some(7)).await.unwrap();
        assert_eq!(repo.retention_days(ws_a).await.unwrap(), Some(7), "A policy persisted");
        assert_eq!(repo.retention_days(ws_b).await.unwrap(), None, "B has no policy");

        let now = time::OffsetDateTime::now_utc();
        let old_a = insert_message_at(&p, room_a, owner_a, now - time::Duration::days(30)).await;
        let old_b = insert_message_at(&p, room_b, owner_b, now - time::Duration::days(30)).await;

        let first = repo.sweep_expired_messages(now, Some(ws_a)).await.unwrap();
        assert_eq!(first.len(), 1, "only A's old message is swept; B is untouched");
        assert!(message_state(&p, old_a).await.0.is_some(), "A's old message swept");
        assert!(message_state(&p, old_b).await.0.is_none(), "B's old message survives");
        // B has no policy: scoping the sweep to B deletes nothing.
        assert_eq!(
            repo.sweep_expired_messages(now, Some(ws_b)).await.unwrap().len(),
            0,
            "unpolicied workspace B is never swept"
        );

        // A second sweep finds nothing new (already-deleted rows are excluded).
        let second = repo.sweep_expired_messages(now, Some(ws_a)).await.unwrap();
        assert_eq!(second.len(), 0, "sweep is idempotent — no double-deletion");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn set_retention_can_clear_back_to_keep_forever() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, _seeded) = seed_workspace(&repo, &p, owner).await;

        repo.set_retention(ws, Some(1)).await.unwrap();
        assert_eq!(repo.retention_days(ws).await.unwrap(), Some(1));
        // Clearing the policy stops the sweep from touching the tenant.
        repo.set_retention(ws, None).await.unwrap();
        assert_eq!(repo.retention_days(ws).await.unwrap(), None);

        let now = time::OffsetDateTime::now_utc();
        let old = insert_message_at(&p, room, owner, now - time::Duration::days(365)).await;
        assert_eq!(repo.sweep_expired_messages(now, Some(ws)).await.unwrap().len(), 0, "cleared policy = no sweep");
        assert!(message_state(&p, old).await.0.is_none(), "message kept after policy cleared");
    }

    /// Insert a bare channel room in `workspace` (no retention override, no
    /// auto-enrolled owner), returning its id. Lets the per-channel override sweep
    /// test place several rooms in one tenant deterministically.
    async fn insert_room(
        p: &PgPool,
        workspace: WorkspaceId,
        creator: ParticipantId,
    ) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'channel', $2, $3, now(), $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("room-{id}"))
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sweep_room_override_takes_precedence_over_workspace_default() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let rooms = crate::room::RoomRepo::new(p.clone());
        let owner = new_participant(&p).await;
        // Workspace default: 30 days. `seed_workspace`'s room inherits it.
        let (ws, inherit_room, _seeded) = seed_workspace(&repo, &p, owner).await;
        repo.set_retention(ws, Some(30)).await.unwrap();

        // A SHORTER override (7 days) and a LONGER override (365 days), both in the
        // same workspace as the inheriting room.
        let short_room = insert_room(&p, ws, owner).await;
        let long_room = insert_room(&p, ws, owner).await;
        rooms.set_retention_days(short_room, Some(7)).await.unwrap();
        rooms.set_retention_days(long_room, Some(365)).await.unwrap();
        assert_eq!(rooms.retention_days(short_room).await.unwrap(), Some(7), "short override persisted");
        assert_eq!(rooms.retention_days(long_room).await.unwrap(), Some(365), "long override persisted");
        assert_eq!(rooms.retention_days(inherit_room).await.unwrap(), None, "no override = inherit");

        let now = time::OffsetDateTime::now_utc();
        // 10 days old: KEPT under the 30-day default, but PAST the 7-day override.
        let in_short = insert_message_at(&p, short_room, owner, now - time::Duration::days(10)).await;
        // 3 days old: within the 7-day override → kept.
        let fresh_short = insert_message_at(&p, short_room, owner, now - time::Duration::days(3)).await;
        // 100 days old: SWEPT under the 30-day default, but WITHIN the 365-day override.
        let in_long = insert_message_at(&p, long_room, owner, now - time::Duration::days(100)).await;
        // 40 days old: swept under the inherited 30-day workspace default.
        let in_inherit = insert_message_at(&p, inherit_room, owner, now - time::Duration::days(40)).await;

        let swept = repo.sweep_expired_messages(now, Some(ws)).await.unwrap();
        assert_eq!(swept.len(), 2, "the short-override 10d message and the inherited 40d message");

        // A shorter override sweeps a message the workspace default would have kept.
        assert!(message_state(&p, in_short).await.0.is_some(), "shorter override swept the 10-day message");
        // Within the shorter override → kept.
        assert!(message_state(&p, fresh_short).await.0.is_none(), "3-day message within 7-day override kept");
        // A longer override keeps a message the workspace default would have swept.
        assert!(message_state(&p, in_long).await.0.is_none(), "longer override kept the 100-day message");
        // No override inherits the workspace default.
        assert!(message_state(&p, in_inherit).await.0.is_some(), "inherited default swept the 40-day message");
    }

    // ----- rate tier (0076) -----

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_rate_tier_defaults_and_updates() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, _room, _msg) = seed_workspace(&repo, &p, owner).await;

        // 0076's column default: every new workspace starts at 'standard'.
        assert_eq!(repo.rate_tier(ws).await.unwrap().as_deref(), Some("standard"));

        assert!(repo.set_rate_tier(ws, "premium").await.unwrap(), "row updated");
        assert_eq!(repo.rate_tier(ws).await.unwrap().as_deref(), Some("premium"));

        assert!(repo.set_rate_tier(ws, "unlimited").await.unwrap());
        assert_eq!(repo.rate_tier(ws).await.unwrap().as_deref(), Some("unlimited"));

        // Unknown workspace: nothing to update, nothing to read.
        assert!(!repo.set_rate_tier(WorkspaceId::new(), "premium").await.unwrap());
        assert_eq!(repo.rate_tier(WorkspaceId::new()).await.unwrap(), None);
    }

    // ----- single-channel guests (0027) -----

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn add_guest_member_flags_and_is_guest_detects() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, _room, _msg) = seed_workspace(&repo, &p, owner).await;
        let guest = new_participant(&p).await;

        // Brand-new participant, not yet a member: neither member nor guest.
        assert!(!repo.is_member(ws, guest).await.unwrap(), "not a member yet");
        assert!(!repo.is_guest(ws, guest).await.unwrap(), "not a guest yet");

        repo.add_guest_member(ws, guest).await.unwrap();
        // Now a member, flagged as a guest, with role stored as `member`.
        assert!(repo.is_member(ws, guest).await.unwrap(), "guest is a member");
        assert!(repo.is_guest(ws, guest).await.unwrap(), "guest flag set");
        assert_eq!(
            repo.member_role(ws, guest).await.unwrap(),
            Some(WorkspaceRole::Member),
            "guest role stays member; guest-ness is a flag"
        );

        // The owner is an ordinary member, never a guest.
        assert!(!repo.is_guest(ws, owner).await.unwrap(), "owner is not a guest");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn add_guest_member_is_idempotent_and_promotes_existing_member() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, _room, _msg) = seed_workspace(&repo, &p, owner).await;
        let member = new_participant(&p).await;

        // Enroll as an ordinary member first, then promote to guest.
        repo.add_member(ws, member, WorkspaceRole::Member).await.unwrap();
        assert!(!repo.is_guest(ws, member).await.unwrap(), "plain member is not a guest");

        repo.add_guest_member(ws, member).await.unwrap();
        assert!(repo.is_guest(ws, member).await.unwrap(), "member promoted to guest");
        // The role is untouched by the conflict-update branch.
        assert_eq!(
            repo.member_role(ws, member).await.unwrap(),
            Some(WorkspaceRole::Member),
            "promotion leaves role unchanged"
        );

        // Re-inviting an existing guest is a harmless no-op.
        repo.add_guest_member(ws, member).await.unwrap();
        assert!(repo.is_guest(ws, member).await.unwrap(), "re-invite keeps guest flag");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn list_guests_returns_only_guests_and_remove_clears_them() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, _room, _msg) = seed_workspace(&repo, &p, owner).await;
        let plain = new_participant(&p).await;
        let guest_a = new_participant(&p).await;
        let guest_b = new_participant(&p).await;

        repo.add_member(ws, plain, WorkspaceRole::Member).await.unwrap();
        repo.add_guest_member(ws, guest_a).await.unwrap();
        repo.add_guest_member(ws, guest_b).await.unwrap();

        let guests = repo.list_guests(ws).await.unwrap();
        let ids: Vec<_> = guests.iter().map(|m| m.participant_id).collect();
        assert!(ids.contains(&guest_a), "guest_a listed");
        assert!(ids.contains(&guest_b), "guest_b listed");
        assert!(!ids.contains(&owner), "owner (member) not listed");
        assert!(!ids.contains(&plain), "plain member not listed");
        assert_eq!(guests.len(), 2, "exactly the two guests");

        // Removing a guest removes their membership entirely.
        repo.remove_member(ws, guest_a).await.unwrap();
        assert!(!repo.is_member(ws, guest_a).await.unwrap(), "removed guest is no longer a member");
        assert!(!repo.is_guest(ws, guest_a).await.unwrap(), "removed guest is no longer a guest");
        let after = repo.list_guests(ws).await.unwrap();
        assert_eq!(after.len(), 1, "one guest left");
        assert_eq!(after[0].participant_id, guest_b, "guest_b remains");
    }

    /// Removing a workspace member also strips their room memberships, so re-adding
    /// them later cannot silently restore access to channels (incl. private ones)
    /// they were previously in.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn remove_member_revokes_room_memberships() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let rooms = crate::room::RoomRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, _) = seed_workspace(&repo, &p, owner).await;

        let bob = new_participant(&p).await;
        repo.add_member(ws, bob, WorkspaceRole::Member).await.unwrap();
        rooms.add_member(room, bob).await.unwrap();
        assert!(rooms.is_member(room, bob).await.unwrap(), "bob starts as a room member");

        repo.remove_member(ws, bob).await.unwrap();
        assert!(!repo.is_member(ws, bob).await.unwrap(), "no longer a workspace member");
        assert!(
            !rooms.is_member(room, bob).await.unwrap(),
            "room membership revoked on workspace removal (no restore on re-add)",
        );
    }
}
