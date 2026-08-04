//! Direct-message (1:1) room lookup.
//!
//! A 1:1 DM is just an ordinary `direct`-kind room with exactly two members. This
//! repo owns both exact lookup and atomic find-or-create. The latter takes a
//! transaction-scoped advisory lock derived from the workspace + sorted pair,
//! rechecks under that lock, and inserts the room and both membership edges in
//! one transaction when absent.
//!
//! It uses the existing `rooms` / `room_members` tables and requires no auxiliary
//! uniqueness table or migration.

use aero_common::{ParticipantId, Room, RoomId, RoomKind, WorkspaceId};
use sqlx::PgPool;

/// Stable failures from the transaction-owned 1:1 DM open operation.
#[derive(Debug, thiserror::Error)]
pub enum DmWriteError {
    #[error("a direct-message member no longer has effective workspace access")]
    Forbidden,
    #[error("an information barrier forbids this direct message")]
    InformationBarrier,
    #[error("a user block forbids this direct message")]
    Blocked,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Repository over `rooms` / `room_members` for 1:1 direct-message lookup.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DmRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DmRepo {
    pool: PgPool,
}

impl DmRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Find the existing 1:1 direct room shared by `a` and `b`, or `None` if there
    /// is none. A match is a `direct`-kind room that both participants belong to
    /// and that has *exactly two* members, so a group DM (3+) never matches. The
    /// lookup is symmetric — `find_direct(a, b)` and `find_direct(b, a)` resolve to
    /// the same room. Rejecting a self-DM (`a == b`) is the caller's concern.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn find_direct(
        &self,
        a: ParticipantId,
        b: ParticipantId,
    ) -> Result<Option<RoomId>, sqlx::Error> {
        self.find_direct_scoped(None, a, b).await
    }

    /// Workspace-scoped counterpart to [`Self::find_direct`].
    pub async fn find_direct_in_workspace(
        &self,
        workspace: WorkspaceId,
        a: ParticipantId,
        b: ParticipantId,
    ) -> Result<Option<RoomId>, sqlx::Error> {
        self.find_direct_scoped(Some(workspace), a, b).await
    }

    async fn find_direct_scoped(
        &self,
        workspace: Option<WorkspaceId>,
        a: ParticipantId,
        b: ParticipantId,
    ) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT rm1.room_id
                FROM room_members rm1
                JOIN room_members rm2 ON rm1.room_id = rm2.room_id
                JOIN rooms r ON r.id = rm1.room_id
               WHERE rm1.participant_id = $1
                 AND rm2.participant_id = $2
                 AND r.kind = 'direct'
                 AND ($3::uuid IS NULL OR r.workspace_id = $3)
                 AND (SELECT count(*) FROM room_members rm3 WHERE rm3.room_id = rm1.room_id) = 2
               ORDER BY rm1.room_id
               LIMIT 1",
        )
        .bind(a.to_uuid())
        .bind(b.to_uuid())
        .bind(workspace.map(|id| id.to_uuid()))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| RoomId::from_uuid(id)))
    }

    /// Atomically find or create the workspace-scoped 1:1 room for `a` and `b`.
    ///
    /// Lock order is workspace → exact-pair advisory → workspace-member rows.
    /// Information-barrier and user-group writers take the workspace row
    /// `FOR UPDATE`, so the in-transaction barrier recheck linearizes with those
    /// compliance writes. The lookup is repeated after acquiring the locks; if
    /// absent, the room and both membership edges are inserted in one transaction.
    pub async fn find_or_create_in_workspace(
        &self,
        workspace: WorkspaceId,
        a: ParticipantId,
        b: ParticipantId,
    ) -> Result<Room, DmWriteError> {
        let (low, high) = if a.to_uuid() <= b.to_uuid() {
            (a, b)
        } else {
            (b, a)
        };
        let lock_key = format!("aero:dm:{workspace}:{low}:{high}");
        let mut tx = self.pool.begin().await?;

        // Workspace first. Barrier creation, user-group membership changes, and
        // workspace access revocation all take the conflicting UPDATE lock.
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            return Err(DmWriteError::Forbidden);
        }

        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(&mut *tx)
            .await?;
        crate::user_blocks::lock_user_block_pair(&mut tx, low, high).await?;
        for participant in [low, high] {
            let effective: bool =
                sqlx::query_scalar("SELECT aero_effective_workspace_access($1, $2)")
                    .bind(workspace.to_uuid())
                    .bind(participant.to_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
            if !effective {
                return Err(DmWriteError::Forbidden);
            }
        }

        let barred: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                  SELECT 1
                    FROM info_barriers barrier
                    JOIN user_group_members side_a
                      ON side_a.group_id = barrier.group_a
                    JOIN user_group_members side_b
                      ON side_b.group_id = barrier.group_b
                   WHERE barrier.workspace_id = $1
                     AND (
                           (
                               side_a.participant_id = $2
                               AND side_b.participant_id = $3
                           )
                           OR (
                               side_a.participant_id = $3
                               AND side_b.participant_id = $2
                           )
                     )
              )",
        )
        .bind(workspace.to_uuid())
        .bind(low.to_uuid())
        .bind(high.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if barred {
            return Err(DmWriteError::InformationBarrier);
        }
        let blocked: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                  SELECT 1
                    FROM user_blocks
                   WHERE (
                             blocker_id = $1
                         AND blocked_id = $2
                         )
                      OR (
                             blocker_id = $2
                         AND blocked_id = $1
                         )
              )",
        )
        .bind(low.to_uuid())
        .bind(high.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if blocked {
            return Err(DmWriteError::Blocked);
        }

        let existing =
            sqlx::query_as::<_, (uuid::Uuid, Option<String>, uuid::Uuid, time::OffsetDateTime)>(
                r"SELECT r.id, r.name, r.created_by, r.created_at
                FROM rooms r
               WHERE r.workspace_id = $1
                 AND r.kind = 'direct'
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $2
                 )
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $3
                 )
                 AND (SELECT count(*) FROM room_members WHERE room_id = r.id) = 2
               ORDER BY r.id
               LIMIT 1",
            )
            .bind(workspace.to_uuid())
            .bind(a.to_uuid())
            .bind(b.to_uuid())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((id, name, created_by, created_at)) = existing {
            tx.commit().await?;
            return Ok(Room {
                id: RoomId::from_uuid(id),
                kind: RoomKind::Direct,
                name,
                created_by: ParticipantId::from_uuid(created_by),
                created_at,
            });
        }

        let room = Room {
            id: RoomId::new(),
            kind: RoomKind::Direct,
            name: None,
            created_by: a,
            created_at: time::OffsetDateTime::now_utc(),
        };
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'direct', NULL, $2, $3, $4)",
        )
        .bind(room.id.to_uuid())
        .bind(a.to_uuid())
        .bind(room.created_at)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', $4), ($1, $3, 'member', $4)",
        )
        .bind(room.id.to_uuid())
        .bind(a.to_uuid())
        .bind(b.to_uuid())
        .bind(room.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(room)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored dm
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the seeded rooms are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(6)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("dm-participant-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn enroll(p: &PgPool, workspace: WorkspaceId, participants: &[ParticipantId]) {
        for participant in participants {
            sqlx::query(
                r"INSERT INTO workspace_members
                      (workspace_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'member', now())",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(p)
            .await
            .expect("enroll workspace member");
        }
    }

    /// Insert a room of the given `kind` in the default workspace and enroll every
    /// `members` participant, so membership/kind/count assertions are deterministic.
    async fn mk_room(p: &PgPool, kind: &str, members: &[ParticipantId]) -> RoomId {
        let id = RoomId::new();
        let creator = members.first().copied().unwrap_or_else(ParticipantId::new);
        let mut tx = p.begin().await.expect("begin room fixture");
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(kind)
        .bind(format!("dm-room-{id}"))
        .bind(creator.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
        .execute(&mut *tx)
        .await
        .expect("insert room");
        for m in members {
            sqlx::query(
                r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'member', now())",
            )
            .bind(id.to_uuid())
            .bind(m.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert room member");
        }
        tx.commit().await.expect("commit room fixture");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn find_direct_matches_two_member_direct_room_both_orders() {
        let pg = pool();
        let repo = DmRepo::new(pg.clone());

        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await; // a third, unrelated participant
        let dan = mk_participant(&pg).await;
        let erin = mk_participant(&pg).await;
        let finn = mk_participant(&pg).await;
        let gabe = mk_participant(&pg).await;
        let hank = mk_participant(&pg).await;

        // The 1:1 DM between alice and bob.
        let dm = mk_room(&pg, "direct", &[alice, bob]).await;
        // A second non-direct group room shared by dan and erin.
        let non_direct = mk_room(&pg, "group", &[dan, erin]).await;
        // A 3-member group room shared by finn, gabe, hank.
        let group = mk_room(&pg, "group", &[finn, gabe, hank]).await;

        // Symmetric match for the real 1:1.
        assert_eq!(
            repo.find_direct(alice, bob).await.unwrap(),
            Some(dm),
            "find_direct(alice, bob) returns the 1:1"
        );
        assert_eq!(
            repo.find_direct(bob, alice).await.unwrap(),
            Some(dm),
            "find_direct is symmetric (bob, alice) returns the same 1:1"
        );

        // A third participant who shares no DM with alice is not matched.
        assert_eq!(
            repo.find_direct(alice, carol).await.unwrap(),
            None,
            "unrelated third participant is not matched"
        );

        // A non-direct room is never matched as a DM.
        assert_eq!(
            repo.find_direct(dan, erin).await.unwrap(),
            None,
            "a non-direct room is not matched"
        );

        // A group room with 3 members is not a 1:1.
        assert_eq!(
            repo.find_direct(finn, gabe).await.unwrap(),
            None,
            "a group room is not matched"
        );

        // Cleanup so reruns stay self-contained.
        for room in [dm, non_direct, group] {
            sqlx::query("DELETE FROM room_members WHERE room_id = $1")
                .bind(room.to_uuid())
                .execute(&pg)
                .await
                .ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
        for who in [alice, bob, carol, dan, erin, finn, gabe, hank] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_find_or_create_returns_one_complete_direct_room() {
        let pg = pool();
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        enroll(&pg, workspace, &[alice, bob]).await;
        let mut opens = Vec::new();
        for _ in 0..8 {
            let repo = DmRepo::new(pg.clone());
            opens.push(tokio::spawn(async move {
                repo.find_or_create_in_workspace(workspace, alice, bob)
                    .await
                    .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for open in opens {
            ids.push(open.await.unwrap().id);
        }
        assert!(ids.iter().all(|id| *id == ids[0]));

        let rooms: i64 = sqlx::query_scalar(
            r"SELECT count(*)
                FROM rooms r
               WHERE r.workspace_id = $1
                 AND r.kind = 'direct'
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $2
                 )
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $3
                 )
                 AND (SELECT count(*) FROM room_members WHERE room_id = r.id) = 2",
        )
        .bind(workspace.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .fetch_one(&pg)
        .await
        .unwrap();
        let members: i64 =
            sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
                .bind(ids[0].to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap();
        assert_eq!(rooms, 1);
        assert_eq!(members, 2);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn waiting_find_or_create_fails_after_member_deactivation_commits() {
        let pg = pool();
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let workspace = WorkspaceId::new();
        let mut workspace_birth = pg.begin().await.unwrap();
        sqlx::query(
            r"INSERT INTO workspaces (id, name, slug, created_by)
               VALUES ($1, 'DM race', $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("dm-race-{workspace}"))
        .bind(alice.to_uuid())
        .execute(&mut *workspace_birth)
        .await
        .unwrap();
        sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', now()),
                      ($1, $3, 'member', now())",
        )
        .bind(workspace.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .execute(&mut *workspace_birth)
        .await
        .unwrap();
        workspace_birth.commit().await.unwrap();
        assert!(crate::WorkspaceRepo::new(pg.clone())
            .all_effective_members(workspace, &[alice, bob])
            .await
            .unwrap());

        // Hold the global first lock and stage revocation in the same
        // transaction. The DM open must wait, then observe the committed
        // deactivation before it can acquire its pair advisory lock.
        let mut blocker = pg.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *blocker)
            .await
            .unwrap();
        sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(bob.to_uuid())
        .bind(alice.to_uuid())
        .execute(&mut *blocker)
        .await
        .unwrap();

        let repo = DmRepo::new(pg.clone());
        let mut waiting = tokio::spawn(async move {
            repo.find_or_create_in_workspace(workspace, alice, bob)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "find-or-create must wait on the workspace revocation lock"
        );
        blocker.commit().await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .expect("waiting create completes")
            .expect("task does not panic");
        assert!(matches!(result, Err(DmWriteError::Forbidden)));
        let rooms: i64 = sqlx::query_scalar(
            r"SELECT count(*)
                FROM rooms r
               WHERE r.workspace_id = $1
                 AND r.kind = 'direct'
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $2
                 )
                 AND EXISTS (
                     SELECT 1 FROM room_members
                      WHERE room_id = r.id AND participant_id = $3
                 )",
        )
        .bind(workspace.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(rooms, 0, "revoked members must not get a new DM");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn barrier_commit_wins_workspace_lock_and_prevents_direct_creation() {
        let pg = pool();
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        enroll(&pg, workspace, &[alice, bob]).await;

        let group_a = uuid::Uuid::new_v4();
        let group_b = uuid::Uuid::new_v4();
        for (group, handle) in [(group_a, "dm-barrier-a"), (group_b, "dm-barrier-b")] {
            sqlx::query(
                r"INSERT INTO user_groups
                      (id, workspace_id, handle, name, created_by)
                   VALUES ($1, $2, $3, $3, $4)",
            )
            .bind(group)
            .bind(workspace.to_uuid())
            .bind(format!("{handle}-{group}"))
            .bind(alice.to_uuid())
            .execute(&pg)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO user_group_members (group_id, participant_id)
             VALUES ($1, $2), ($3, $4)",
        )
        .bind(group_a)
        .bind(alice.to_uuid())
        .bind(group_b)
        .bind(bob.to_uuid())
        .execute(&pg)
        .await
        .unwrap();

        let mut barrier_tx = pg.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *barrier_tx)
            .await
            .unwrap();
        let barrier_id = uuid::Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO info_barriers
                  (id, workspace_id, group_a, group_b, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(barrier_id)
        .bind(workspace.to_uuid())
        .bind(group_a)
        .bind(group_b)
        .bind(alice.to_uuid())
        .execute(&mut *barrier_tx)
        .await
        .unwrap();

        let racing_repo = DmRepo::new(pg.clone());
        let mut open = tokio::spawn(async move {
            racing_repo
                .find_or_create_in_workspace(workspace, alice, bob)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut open)
                .await
                .is_err(),
            "direct open must wait behind a barrier writer's workspace lock"
        );
        barrier_tx.commit().await.unwrap();
        assert!(matches!(
            open.await.unwrap(),
            Err(DmWriteError::InformationBarrier)
        ));

        let created: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM rooms WHERE created_by = $1 AND kind='direct'",
        )
        .bind(alice.to_uuid())
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(created, 0);

        sqlx::query("DELETE FROM info_barriers WHERE id = $1")
            .bind(barrier_id)
            .execute(&pg)
            .await
            .ok();
        for group in [group_a, group_b] {
            sqlx::query("DELETE FROM user_groups WHERE id = $1")
                .bind(group)
                .execute(&pg)
                .await
                .ok();
        }
        for participant in [alice, bob] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_direct_appends_cannot_cross_two_member_fence() {
        let pg = pool();
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        let dan = mk_participant(&pg).await;
        enroll(&pg, workspace, &[alice, bob, carol, dan]).await;

        let room = RoomId::new();
        let mut birth = pg.begin().await.unwrap();
        sqlx::query(
            r"INSERT INTO rooms
                  (id, kind, name, created_by, workspace_id, is_group_dm)
               VALUES ($1, 'direct', NULL, $2, $3, false)",
        )
        .bind(room.to_uuid())
        .bind(alice.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *birth)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
        )
        .bind(room.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .execute(&mut *birth)
        .await
        .unwrap();
        birth.commit().await.unwrap();

        // Make both INSERT statements establish their trigger execution before
        // either can inspect the fixed set under the room lock.
        let mut blocker = pg.begin().await.unwrap();
        sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(room.to_uuid())
            .execute(&mut *blocker)
            .await
            .unwrap();

        let append = |participant: ParticipantId| {
            let pool = pg.clone();
            tokio::spawn(async move {
                sqlx::query(
                    "INSERT INTO room_members (room_id, participant_id, role)
                     VALUES ($1, $2, 'member')",
                )
                .bind(room.to_uuid())
                .bind(participant.to_uuid())
                .execute(&pool)
                .await
            })
        };
        let carol_append = append(carol);
        let dan_append = append(dan);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        blocker.commit().await.unwrap();

        let results = [carol_append.await.unwrap(), dan_append.await.unwrap()];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            0,
            "neither concurrent third member may commit"
        );
        for rejected in results.iter().filter_map(|result| result.as_ref().err()) {
            assert_eq!(
                rejected
                    .as_database_error()
                    .and_then(|error| error.constraint()),
                Some("room_members_fixed_membership_insert")
            );
        }
        let member_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
                .bind(room.to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap();
        assert_eq!(member_count, 2);

        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&pg)
            .await
            .ok();
        for participant in [alice, bob, carol] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }
}

#[cfg(test)]
#[path = "dm/block_tests.rs"]
mod block_tests;
