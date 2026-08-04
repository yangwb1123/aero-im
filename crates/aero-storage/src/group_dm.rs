//! Group direct-message (multi-person) room lookup.
//!
//! A group DM is a small fixed-member conversation identified by
//! `rooms.is_group_dm`, independently of its mutable display name. This repo owns
//! exact lookup, listing, atomic find-or-create, and marker-aware metadata updates.
//! Find-or-create locks on the workspace + sorted exact member set, rechecks under
//! that transaction-scoped lock, and inserts the room plus every membership edge
//! atomically when absent.

use aero_common::{ParticipantId, Room, RoomId, RoomKind, WorkspaceId};
use sqlx::{PgPool, Postgres, Transaction};

use crate::room::MAX_CHANNEL_DESCRIPTION_CHARS;

/// Maximum mutable group-DM display-name length, in Unicode scalar values.
pub const MAX_GROUP_DM_NAME_CHARS: usize = 128;
const MIN_GROUP_DM_MEMBERS: usize = 3;
const MAX_GROUP_DM_MEMBERS: usize = 8;

/// A marker-aware group-DM metadata mutation failure.
#[derive(Debug, thiserror::Error)]
pub enum GroupDmWriteError {
    #[error("group DM room not found")]
    NotFound,
    #[error("room is not a group DM")]
    NotGroupDm,
    #[error("caller no longer has effective access to the group DM")]
    Forbidden,
    #[error("an information barrier forbids this group DM")]
    InformationBarrier,
    #[error("invalid group DM metadata: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Repository over `rooms` / `room_members` for persistent group-DM lookup.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`GroupDmRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct GroupDmRepo {
    pool: PgPool,
}

impl GroupDmRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Find the group DM in `workspace` whose member set is **exactly**
    /// `members`, or `None` if there is none. The room's mutable name is
    /// deliberately irrelevant. Neither a subset nor a superset matches, and
    /// duplicate input ids are folded before comparison.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn find_exact_in_workspace(
        &self,
        workspace: WorkspaceId,
        members: &[ParticipantId],
    ) -> Result<Option<RoomId>, sqlx::Error> {
        // Sort the member uuids so the bound array matches the room's
        // `array_agg(... ORDER BY participant_id)` ordering for equality.
        let mut ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
        ids.sort_unstable();
        ids.dedup();
        let count = i64::try_from(ids.len()).unwrap_or(i64::MAX);

        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT rm.room_id FROM room_members rm JOIN rooms r ON r.id = rm.room_id
               WHERE r.kind = 'group'
                 AND r.is_group_dm
                 AND r.workspace_id = $3
               GROUP BY rm.room_id
               HAVING count(*) = $2
                  AND array_agg(rm.participant_id ORDER BY rm.participant_id) = $1
               ORDER BY rm.room_id
               LIMIT 1",
        )
        .bind(&ids)
        .bind(count)
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| RoomId::from_uuid(id)))
    }

    /// List the participant's currently accessible group DMs in one workspace.
    ///
    /// The effective-access joins mirror [`crate::RoomRepo::rooms_for_in_workspace`]
    /// so a deactivated/deleted participant or one missing mandatory 2FA cannot
    /// regain data through a race after the HTTP preflight guard.
    pub async fn list_for_participant_in_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<Room>, sqlx::Error> {
        let rows =
            sqlx::query_as::<_, (uuid::Uuid, Option<String>, uuid::Uuid, time::OffsetDateTime)>(
                r"SELECT r.id, r.name, r.created_by, r.created_at
                FROM rooms r
                JOIN room_members member
                  ON member.room_id = r.id
                JOIN workspaces workspace
                  ON workspace.id = r.workspace_id
                JOIN workspace_members workspace_member
                  ON workspace_member.workspace_id = r.workspace_id
                 AND workspace_member.participant_id = member.participant_id
                JOIN participants participant
                  ON participant.id = member.participant_id
                 AND participant.deleted_at IS NULL
                LEFT JOIN workspace_deactivations deactivated
                  ON deactivated.workspace_id = r.workspace_id
                 AND deactivated.participant_id = member.participant_id
                LEFT JOIN totp_secrets totp
                  ON totp.participant_id = member.participant_id
               WHERE member.participant_id = $1
                 AND r.workspace_id = $2
                 AND r.kind = 'group'
                 AND r.is_group_dm
                 AND deactivated.participant_id IS NULL
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
               ORDER BY r.created_at DESC",
            )
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(id, name, created_by, created_at)| Room {
                id: RoomId::from_uuid(id),
                kind: RoomKind::Group,
                name,
                created_by: ParticipantId::from_uuid(created_by),
                created_at,
            })
            .collect())
    }

    /// Atomically find or create a group DM for an exact member set.
    ///
    /// Lock order is workspace → rolling-compatibility advisory → exact-set
    /// advisory → workspace-member rows → room/member rows. Competing first-open
    /// requests therefore serialize, and information-barrier / user-group writes
    /// (which take the workspace row `FOR UPDATE`) linearize against the in-
    /// transaction barrier recheck.
    ///
    /// During the expand side of a rolling deployment, an old pod may create an
    /// unmarked, nameless `group` room with the exact requested set. Such a room
    /// is locked, revalidated, and atomically claimed by setting
    /// `is_group_dm = true`; no duplicate marked room is created. New rooms are
    /// built unmarked, receive their complete edge set, and are marked only as
    /// the final transaction write so migration 0196's old-writer fence does not
    /// reject their own construction.
    pub async fn find_or_create_in_workspace(
        &self,
        workspace: WorkspaceId,
        members: &[ParticipantId],
        creator: ParticipantId,
    ) -> Result<Room, GroupDmWriteError> {
        let mut ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
        ids.sort_unstable();
        ids.dedup();
        if !(MIN_GROUP_DM_MEMBERS..=MAX_GROUP_DM_MEMBERS).contains(&ids.len()) {
            return Err(GroupDmWriteError::InvalidInput(format!(
                "a group DM must contain {MIN_GROUP_DM_MEMBERS}..={MAX_GROUP_DM_MEMBERS} distinct members"
            )));
        }
        if ids.binary_search(&creator.to_uuid()).is_err() {
            return Err(GroupDmWriteError::InvalidInput(
                "the group-DM creator must belong to its fixed member set".into(),
            ));
        }
        let member_key = ids
            .iter()
            .map(uuid::Uuid::to_string)
            .collect::<Vec<_>>()
            .join(":");
        let workspace_uuid = workspace.to_uuid();
        let compatibility_lock_key = format!("aero:group-dm-legacy:{workspace_uuid}");
        let exact_lock_key = format!("aero:group-dm:{workspace_uuid}:{member_key}");

        let mut tx = self.pool.begin().await?;

        // Workspace first: barrier creation, user-group membership changes, and
        // workspace access revocation all take this same row FOR UPDATE.
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
                .bind(workspace_uuid)
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            return Err(GroupDmWriteError::Forbidden);
        }

        // Serialize old pods' incremental, unmarked room-member writes with new
        // exact lookup/create. The trigger installed by migration 0196 takes the
        // same key after the workspace row.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(compatibility_lock_key)
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(exact_lock_key)
            .execute(&mut *tx)
            .await?;

        for participant in &ids {
            let effective: bool =
                sqlx::query_scalar("SELECT aero_effective_workspace_access($1, $2)")
                    .bind(workspace_uuid)
                    .bind(participant)
                    .fetch_one(&mut *tx)
                    .await?;
            if !effective {
                return Err(GroupDmWriteError::Forbidden);
            }
        }

        // This MUST remain inside the workspace lock. Barrier and user-group
        // writers take the conflicting workspace UPDATE lock, so either this
        // check/create commits first or their change commits first and is seen.
        let barred: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                  SELECT 1
                    FROM info_barriers barrier
                    JOIN user_group_members side_a
                      ON side_a.group_id = barrier.group_a
                    JOIN user_group_members side_b
                      ON side_b.group_id = barrier.group_b
                   WHERE barrier.workspace_id = $1
                     AND side_a.participant_id = ANY($2)
                     AND side_b.participant_id = ANY($2)
                     AND side_a.participant_id <> side_b.participant_id
              )",
        )
        .bind(workspace_uuid)
        .bind(&ids)
        .fetch_one(&mut *tx)
        .await?;
        if barred {
            return Err(GroupDmWriteError::InformationBarrier);
        }

        if let Some(existing) = claim_exact_room_in_tx(&mut tx, workspace, &ids).await? {
            tx.commit().await?;
            return Ok(existing);
        }

        let room = Room {
            id: RoomId::new(),
            kind: RoomKind::Group,
            name: None,
            created_by: creator,
            created_at: time::OffsetDateTime::now_utc(),
        };
        sqlx::query(
            r"INSERT INTO rooms
                  (id, kind, name, created_by, created_at, workspace_id, is_group_dm)
               VALUES ($1, 'group', NULL, $2, $3, $4, false)",
        )
        .bind(room.id.to_uuid())
        .bind(creator.to_uuid())
        .bind(room.created_at)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               SELECT $1, member_id,
                      CASE WHEN member_id = $2 THEN 'owner' ELSE 'member' END,
                      $3
                 FROM unnest($4::uuid[]) member_id",
        )
        .bind(room.id.to_uuid())
        .bind(creator.to_uuid())
        .bind(room.created_at)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        let marked = sqlx::query(
            "UPDATE rooms
                SET is_group_dm = true
              WHERE id = $1 AND kind = 'group' AND NOT is_group_dm",
        )
        .bind(room.id.to_uuid())
        .execute(&mut *tx)
        .await?;
        if marked.rows_affected() != 1 {
            return Err(GroupDmWriteError::Storage(sqlx::Error::Protocol(
                "new group-DM marker transition did not update exactly one room".into(),
            )));
        }
        tx.commit().await?;
        Ok(room)
    }

    /// Atomically patch mutable group-DM presentation metadata.
    ///
    /// The outer `Option` distinguishes an omitted field from a present field;
    /// the inner `Option` clears a nullable field. The transaction resolves the
    /// immutable tenant edge without a lock, then follows the global workspace →
    /// room → room-membership lock order before the update. This closes both the
    /// HTTP preflight/revocation race and the former room → workspace deadlock.
    pub async fn patch_metadata(
        &self,
        room: RoomId,
        caller: ParticipantId,
        name: Option<Option<&str>>,
        description: Option<Option<&str>>,
    ) -> Result<(), GroupDmWriteError> {
        if name.is_none() && description.is_none() {
            return Err(GroupDmWriteError::InvalidInput(
                "at least one of name or description is required".into(),
            ));
        }
        if name
            .flatten()
            .is_some_and(|value| value.chars().count() > MAX_GROUP_DM_NAME_CHARS)
        {
            return Err(GroupDmWriteError::InvalidInput(format!(
                "name is too long (max {MAX_GROUP_DM_NAME_CHARS} chars)"
            )));
        }
        if description
            .flatten()
            .is_some_and(|value| value.chars().count() > MAX_CHANNEL_DESCRIPTION_CHARS)
        {
            return Err(GroupDmWriteError::InvalidInput(format!(
                "description is too long (max {MAX_CHANNEL_DESCRIPTION_CHARS} chars)"
            )));
        }

        let name_present = name.is_some();
        let name = name.flatten();
        let description_present = description.is_some();
        let description = description.flatten();
        let mut tx = self.pool.begin().await?;

        // Resolve the immutable room -> workspace edge without taking a room
        // lock. Governance writers acquire the workspace fence first.
        let resolved = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
            "SELECT workspace_id, kind, is_group_dm
               FROM rooms
              WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((resolved_workspace, resolved_kind, resolved_is_group_dm)) = resolved else {
            return Err(GroupDmWriteError::NotFound);
        };
        if resolved_kind != "group" || !resolved_is_group_dm {
            return Err(GroupDmWriteError::NotGroupDm);
        }
        let workspace = WorkspaceId::from_uuid(resolved_workspace);

        if !crate::workspace::members::effective_workspace_access_in_tx(&mut tx, workspace, caller)
            .await?
        {
            return Err(GroupDmWriteError::Forbidden);
        }

        // Lock and revalidate the room only after the canonical workspace
        // boundary is fenced. Room tenant identity is immutable in supported
        // writes; a mismatch therefore indicates an integrity violation.
        let locked = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
            "SELECT workspace_id, kind, is_group_dm
               FROM rooms
              WHERE id = $1
              FOR UPDATE",
        )
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((locked_workspace, locked_kind, locked_is_group_dm)) = locked else {
            return Err(GroupDmWriteError::NotFound);
        };
        if locked_workspace != resolved_workspace {
            return Err(GroupDmWriteError::Storage(sqlx::Error::Protocol(
                "group-DM workspace identity changed concurrently".into(),
            )));
        }
        if locked_kind != "group" || !locked_is_group_dm {
            return Err(GroupDmWriteError::NotGroupDm);
        }

        let room_member = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM room_members
              WHERE room_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(room.to_uuid())
        .bind(caller.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !room_member {
            return Err(GroupDmWriteError::Forbidden);
        }

        let changed = sqlx::query(
            "UPDATE rooms
                SET name = CASE WHEN $2 THEN $3 ELSE name END,
                    description = CASE WHEN $4 THEN $5 ELSE description END
              WHERE id = $1",
        )
        .bind(room.to_uuid())
        .bind(name_present)
        .bind(name)
        .bind(description_present)
        .bind(description)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(GroupDmWriteError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Resolve a marked exact room or claim one completed legacy room.
///
/// The caller owns the workspace + compatibility + exact-set locks. Candidate
/// room and membership rows are still locked and re-read here so an old writer
/// that began before those locks cannot change the set between the aggregate
/// lookup and marker transition.
async fn claim_exact_room_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    expected_members: &[uuid::Uuid],
) -> Result<Option<Room>, GroupDmWriteError> {
    let expected_count = i64::try_from(expected_members.len()).unwrap_or(i64::MAX);
    let candidate_ids = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT room.id
            FROM rooms room
            JOIN room_members member ON member.room_id = room.id
           WHERE room.workspace_id = $1
             AND room.kind = 'group'
             AND (
                 room.is_group_dm
                 OR (NOT room.is_group_dm AND room.name IS NULL)
             )
           GROUP BY room.id, room.is_group_dm
          HAVING count(*) = $3
             AND array_agg(
                     member.participant_id
                     ORDER BY member.participant_id
                 ) = $2
           ORDER BY room.is_group_dm DESC, room.id
           LIMIT 2",
    )
    .bind(workspace.to_uuid())
    .bind(expected_members)
    .bind(expected_count)
    .fetch_all(&mut **tx)
    .await?;

    for candidate_id in candidate_ids {
        let state = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                uuid::Uuid,
                time::OffsetDateTime,
                uuid::Uuid,
                bool,
            ),
        >(
            r"SELECT kind, name, created_by, created_at, workspace_id, is_group_dm
                FROM rooms
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(candidate_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((kind, name, created_by, created_at, room_workspace, is_group_dm)) = state else {
            continue;
        };
        if room_workspace != workspace.to_uuid()
            || kind != "group"
            || (!is_group_dm && name.is_some())
        {
            continue;
        }

        let locked_members = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT participant_id
                FROM room_members
               WHERE room_id = $1
               ORDER BY participant_id
               FOR UPDATE",
        )
        .bind(candidate_id)
        .fetch_all(&mut **tx)
        .await?;
        if locked_members != expected_members {
            continue;
        }

        if !is_group_dm {
            let claimed = sqlx::query(
                "UPDATE rooms
                    SET is_group_dm = true
                  WHERE id = $1
                    AND workspace_id = $2
                    AND kind = 'group'
                    AND NOT is_group_dm
                    AND name IS NULL",
            )
            .bind(candidate_id)
            .bind(workspace.to_uuid())
            .execute(&mut **tx)
            .await?;
            if claimed.rows_affected() != 1 {
                continue;
            }
        }

        return Ok(Some(Room {
            id: RoomId::from_uuid(candidate_id),
            kind: RoomKind::Group,
            name,
            created_by: ParticipantId::from_uuid(created_by),
            created_at,
        }));
    }
    Ok(None)
}

#[cfg(test)]
#[path = "group_dm/metadata_tests.rs"]
mod metadata_tests;

#[cfg(test)]
#[path = "group_dm/fixed_conversation_tests.rs"]
mod fixed_conversation_tests;

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored group_dm
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
            .bind(format!("group-dm-participant-{id}"))
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

    /// Insert a `group`-kind room in the default workspace and enroll every member.
    async fn mk_group_room(
        p: &PgPool,
        name: Option<&str>,
        is_group_dm: bool,
        members: &[ParticipantId],
    ) -> RoomId {
        let id = RoomId::new();
        let creator = members.first().copied().unwrap_or_else(ParticipantId::new);
        sqlx::query(
            r"INSERT INTO rooms
                  (id, kind, name, created_by, workspace_id, is_group_dm)
               VALUES ($1, 'group', $2, $3, $4, false)",
        )
        .bind(id.to_uuid())
        .bind(name)
        .bind(creator.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
        .execute(p)
        .await
        .expect("insert group room");
        for m in members {
            sqlx::query(
                r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'member', now())",
            )
            .bind(id.to_uuid())
            .bind(m.to_uuid())
            .execute(p)
            .await
            .expect("insert room member");
        }
        if is_group_dm {
            sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
                .bind(id.to_uuid())
                .execute(p)
                .await
                .expect("mark group DM after complete membership");
        }
        id
    }

    #[tokio::test]
    async fn metadata_storage_seam_rejects_invalid_input_before_io() {
        let repo = GroupDmRepo::new(pool());
        let room = RoomId::new();
        let caller = ParticipantId::new();
        let too_long_name = "界".repeat(MAX_GROUP_DM_NAME_CHARS + 1);
        assert!(matches!(
            repo.patch_metadata(room, caller, Some(Some(&too_long_name)), None)
                .await,
            Err(GroupDmWriteError::InvalidInput(_))
        ));
        assert!(matches!(
            repo.patch_metadata(room, caller, None, None).await,
            Err(GroupDmWriteError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn marker_survives_naming_and_isolates_ordinary_groups() {
        let pg = pool();
        let repo = GroupDmRepo::new(pg.clone());
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));

        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        enroll(&pg, workspace, &[alice, bob, carol]).await;

        let group_dm = mk_group_room(&pg, None, true, &[alice, bob, carol]).await;
        // Same kind, name state, and exact membership — only the durable marker
        // distinguishes this ordinary group room.
        let ordinary_group = mk_group_room(&pg, None, false, &[alice, bob, carol]).await;

        // The exact set matches, regardless of input order.
        assert_eq!(
            repo.find_exact_in_workspace(workspace, &[alice, bob, carol])
                .await
                .unwrap(),
            Some(group_dm),
            "the marker selects the group DM"
        );
        assert_eq!(
            repo.find_exact_in_workspace(workspace, &[carol, alice, bob, alice])
                .await
                .unwrap(),
            Some(group_dm),
            "find_exact is order-independent and folds duplicate inputs"
        );

        repo.patch_metadata(
            group_dm,
            alice,
            Some(Some("Named conversation")),
            Some(Some(" durable identity ")),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.find_exact_in_workspace(workspace, &[alice, bob, carol])
                .await
                .unwrap(),
            Some(group_dm),
            "naming a group DM does not change its identity"
        );
        let listed = repo
            .list_for_participant_in_workspace(alice, workspace)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, group_dm);
        assert_eq!(listed[0].name.as_deref(), Some("Named conversation"));

        assert!(matches!(
            repo.patch_metadata(ordinary_group, alice, Some(Some("must not change")), None)
                .await,
            Err(GroupDmWriteError::NotGroupDm)
        ));
        let ordinary_name: Option<String> =
            sqlx::query_scalar("SELECT name FROM rooms WHERE id = $1")
                .bind(ordinary_group.to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap();
        assert_eq!(ordinary_name, None);

        sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .execute(&pg)
        .await
        .unwrap();
        assert!(matches!(
            repo.patch_metadata(
                group_dm,
                alice,
                Some(Some("revoked caller must not write")),
                None
            )
            .await,
            Err(GroupDmWriteError::Forbidden)
        ));
        let retained_name: Option<String> =
            sqlx::query_scalar("SELECT name FROM rooms WHERE id = $1")
                .bind(group_dm.to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap();
        assert_eq!(retained_name.as_deref(), Some("Named conversation"));
        sqlx::query(
            "DELETE FROM workspace_deactivations WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(alice.to_uuid())
        .execute(&pg)
        .await
        .unwrap();

        // A 2-of-3 subset is not a match (cardinality differs).
        assert_eq!(
            repo.find_exact_in_workspace(workspace, &[alice, bob])
                .await
                .unwrap(),
            None,
            "a strict subset is not matched"
        );
        // A superset (adds a fourth) is not a match either.
        let dan = mk_participant(&pg).await;
        assert_eq!(
            repo.find_exact_in_workspace(workspace, &[alice, bob, carol, dan])
                .await
                .unwrap(),
            None,
            "a superset is not matched"
        );

        // Cleanup so reruns stay self-contained.
        for room in [group_dm, ordinary_group] {
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
        for who in [alice, bob, carol, dan] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_find_or_create_returns_one_complete_group_dm() {
        let pg = pool();
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        let members = vec![alice, bob, carol];
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        enroll(&pg, workspace, &members).await;

        let mut opens = Vec::new();
        for _ in 0..8 {
            let repo = GroupDmRepo::new(pg.clone());
            let members = members.clone();
            opens.push(tokio::spawn(async move {
                repo.find_or_create_in_workspace(workspace, &members, alice)
                    .await
                    .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for open in opens {
            ids.push(open.await.unwrap().id);
        }
        assert!(ids.iter().all(|id| *id == ids[0]));

        let mut member_ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
        member_ids.sort_unstable();
        let rooms: i64 = sqlx::query_scalar(
            r"SELECT count(*) FROM (
                   SELECT r.id
                     FROM rooms r
                     JOIN room_members rm ON rm.room_id = r.id
                    WHERE r.workspace_id = $1
                      AND r.kind = 'group'
                      AND r.is_group_dm
                    GROUP BY r.id
                   HAVING count(*) = $3
                      AND array_agg(rm.participant_id ORDER BY rm.participant_id) = $2
               ) exact_rooms",
        )
        .bind(workspace.to_uuid())
        .bind(&member_ids)
        .bind(i64::try_from(member_ids.len()).unwrap())
        .fetch_one(&pg)
        .await
        .unwrap();
        let edges: i64 = sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(ids[0].to_uuid())
            .fetch_one(&pg)
            .await
            .unwrap();
        assert_eq!(rooms, 1);
        assert_eq!(edges, 3);

        let repo = GroupDmRepo::new(pg.clone());
        repo.patch_metadata(ids[0], alice, Some(Some("named after creation")), None)
            .await
            .unwrap();
        let reopened = repo
            .find_or_create_in_workspace(workspace, &members, alice)
            .await
            .unwrap();
        assert_eq!(
            reopened.id, ids[0],
            "a named group DM remains the exact-set identity"
        );

        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(ids[0].to_uuid())
            .execute(&pg)
            .await
            .ok();
        for who in members {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn barrier_commit_wins_workspace_lock_and_prevents_group_dm_creation() {
        let pg = pool();
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        let members = vec![alice, bob, carol];
        enroll(&pg, workspace, &members).await;

        let group_a = uuid::Uuid::new_v4();
        let group_b = uuid::Uuid::new_v4();
        for (group, handle) in [(group_a, "barrier-a"), (group_b, "barrier-b")] {
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

        // Hold the same workspace UPDATE lock used by BarrierRepo's authorized
        // writer, persist the barrier, and prove group-DM open waits behind it.
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

        let racing_repo = GroupDmRepo::new(pg.clone());
        let racing_members = members.clone();
        let mut open = tokio::spawn(async move {
            racing_repo
                .find_or_create_in_workspace(workspace, &racing_members, alice)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut open)
                .await
                .is_err(),
            "group-DM open must wait behind a barrier writer's workspace lock"
        );
        barrier_tx.commit().await.unwrap();

        assert!(matches!(
            open.await.unwrap(),
            Err(GroupDmWriteError::InformationBarrier)
        ));
        let created: i64 =
            sqlx::query_scalar("SELECT count(*) FROM rooms WHERE created_by = $1 AND is_group_dm")
                .bind(alice.to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap();
        assert_eq!(created, 0, "barrier-winning race must not create a room");

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
        for participant in members {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn completed_old_pod_room_is_claimed_without_duplicate() {
        let pg = pool();
        let repo = GroupDmRepo::new(pg.clone());
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        let members = vec![alice, bob, carol];
        enroll(&pg, workspace, &members).await;

        // Exact shape written by an old pod after migration 0193: the new column
        // receives its false default while the old code creates a nameless group
        // and appends the complete member set.
        let legacy = mk_group_room(&pg, None, false, &members).await;
        let opened = repo
            .find_or_create_in_workspace(workspace, &members, alice)
            .await
            .unwrap();
        assert_eq!(opened.id, legacy);
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT is_group_dm FROM rooms WHERE id = $1")
                .bind(legacy.to_uuid())
                .fetch_one(&pg)
                .await
                .unwrap(),
            "the exact legacy room must be claimed atomically"
        );

        let mut expected: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
        expected.sort_unstable();
        let exact_rooms: i64 = sqlx::query_scalar(
            r"SELECT count(*)
                FROM (
                    SELECT room.id
                      FROM rooms room
                      JOIN room_members member ON member.room_id = room.id
                     WHERE room.workspace_id = $1
                       AND room.kind = 'group'
                     GROUP BY room.id
                    HAVING count(*) = $3
                       AND array_agg(
                               member.participant_id
                               ORDER BY member.participant_id
                           ) = $2
                ) exact",
        )
        .bind(workspace.to_uuid())
        .bind(&expected)
        .bind(i64::try_from(expected.len()).unwrap())
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(exact_rooms, 1, "claim must not create a duplicate room");

        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(legacy.to_uuid())
            .execute(&pg)
            .await
            .ok();
        for participant in members {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn database_fence_rejects_group_dm_append_and_direct_third_member() {
        let pg = pool();
        let workspace =
            WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"));
        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;
        let dan = mk_participant(&pg).await;
        let participants = vec![alice, bob, carol, dan];
        enroll(&pg, workspace, &participants).await;

        let group_dm = mk_group_room(&pg, None, true, &[alice, bob, carol]).await;
        let group_error = sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(group_dm.to_uuid())
        .bind(dan.to_uuid())
        .execute(&pg)
        .await
        .unwrap_err();
        assert_eq!(
            group_error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("room_members_fixed_membership_insert")
        );

        let direct = RoomId::new();
        let mut direct_birth = pg.begin().await.unwrap();
        sqlx::query(
            r"INSERT INTO rooms
                  (id, kind, name, created_by, workspace_id, is_group_dm)
               VALUES ($1, 'direct', NULL, $2, $3, false)",
        )
        .bind(direct.to_uuid())
        .bind(alice.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *direct_birth)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
        )
        .bind(direct.to_uuid())
        .bind(alice.to_uuid())
        .bind(bob.to_uuid())
        .execute(&mut *direct_birth)
        .await
        .unwrap();
        direct_birth.commit().await.unwrap();
        let direct_error = sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(direct.to_uuid())
        .bind(carol.to_uuid())
        .execute(&pg)
        .await
        .unwrap_err();
        assert_eq!(
            direct_error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("room_members_fixed_membership_insert")
        );
        let counts = sqlx::query_as::<_, (i64, i64)>(
            r"SELECT
                 (SELECT count(*) FROM room_members WHERE room_id = $1),
                 (SELECT count(*) FROM room_members WHERE room_id = $2)",
        )
        .bind(group_dm.to_uuid())
        .bind(direct.to_uuid())
        .fetch_one(&pg)
        .await
        .unwrap();
        assert_eq!(counts, (3, 2));

        for room in [group_dm, direct] {
            sqlx::query("DELETE FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
        for participant in participants {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }
}
