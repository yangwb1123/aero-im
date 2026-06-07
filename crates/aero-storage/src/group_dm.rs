//! Group direct-message (multi-person, nameless) room lookup.
//!
//! A group DM is an ordinary `group`-kind room with **no name** and a small set of
//! members (Slack/Lark "multi-person DM"). This repo owns the single "does a
//! nameless group room already exist among *exactly* this set of people?" query
//! that backs find-or-create: the HTTP layer calls [`GroupDmRepo::find_exact`]
//! first and only falls back to creating a room (via the existing
//! [`RoomRepo`](crate::RoomRepo)) when it returns `None`.
//!
//! Purely additive: a NEW [`GroupDmRepo`] over the EXISTING `rooms` /
//! `room_members` tables; no existing repo is touched, and room *creation* is
//! deliberately left to [`RoomRepo`](crate::RoomRepo) so the create + enroll-owner
//! SQL lives in one place. This repo carries no model struct — it returns the bare
//! [`RoomId`] of the matching group DM.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;

/// Repository over `rooms` / `room_members` for nameless group-DM lookup.
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

    /// Find the existing nameless group room whose member set is **exactly**
    /// `members`, or `None` if there is none. A match is a `group`-kind room with a
    /// `NULL` name whose full membership equals the given set — neither a subset nor
    /// a superset matches, so a group DM is reused only when the participants line
    /// up precisely. The lookup is order-independent: the input ids are sorted and
    /// compared against the room's `array_agg`-sorted membership.
    ///
    /// De-duplicating the input (so its cardinality matches the `room_members` count
    /// check) is the caller's concern; `room_members` is unique per
    /// `(room_id, participant_id)`, so a deduped, sorted set is what the DB side
    /// produces.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn find_exact(
        &self,
        members: &[ParticipantId],
    ) -> Result<Option<RoomId>, sqlx::Error> {
        // Sort the member uuids so the bound array matches the room's
        // `array_agg(... ORDER BY participant_id)` ordering for equality.
        let mut ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
        ids.sort_unstable();
        let count = i64::try_from(ids.len()).unwrap_or(i64::MAX);

        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT rm.room_id FROM room_members rm JOIN rooms r ON r.id = rm.room_id
               WHERE r.kind = 'group' AND r.name IS NULL
               GROUP BY rm.room_id
               HAVING count(*) = $2
                  AND array_agg(rm.participant_id ORDER BY rm.participant_id) = $1
               LIMIT 1",
        )
        .bind(&ids)
        .bind(count)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| RoomId::from_uuid(id)))
    }
}

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
            .max_connections(2)
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

    /// Insert a `group`-kind room with the given (nullable) name in the default
    /// workspace and enroll every `members` participant, so the membership/name/
    /// count assertions are deterministic.
    async fn mk_group_room(p: &PgPool, name: Option<&str>, members: &[ParticipantId]) -> RoomId {
        let id = RoomId::new();
        let creator = members.first().copied().unwrap_or_else(ParticipantId::new);
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
               VALUES ($1, 'group', $2, $3, $4)",
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
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn find_exact_matches_only_the_full_nameless_set() {
        let pg = pool();
        let repo = GroupDmRepo::new(pg.clone());

        let alice = mk_participant(&pg).await;
        let bob = mk_participant(&pg).await;
        let carol = mk_participant(&pg).await;

        // The nameless 3-person group DM.
        let group = mk_group_room(&pg, None, &[alice, bob, carol]).await;
        // A NAMED group room with the very same members — must NOT match.
        let named = mk_group_room(&pg, Some("project x"), &[alice, bob, carol]).await;

        // The exact set matches, regardless of input order.
        assert_eq!(
            repo.find_exact(&[alice, bob, carol]).await.unwrap(),
            Some(group),
            "the exact 3-person set matches the nameless group DM"
        );
        assert_eq!(
            repo.find_exact(&[carol, alice, bob]).await.unwrap(),
            Some(group),
            "find_exact is order-independent"
        );

        // A 2-of-3 subset is not a match (cardinality differs).
        assert_eq!(
            repo.find_exact(&[alice, bob]).await.unwrap(),
            None,
            "a strict subset is not matched"
        );
        // A superset (adds a fourth) is not a match either.
        let dan = mk_participant(&pg).await;
        assert_eq!(
            repo.find_exact(&[alice, bob, carol, dan]).await.unwrap(),
            None,
            "a superset is not matched"
        );

        // Cleanup so reruns stay self-contained.
        for room in [group, named] {
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
}
