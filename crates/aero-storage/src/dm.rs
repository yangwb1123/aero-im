//! Direct-message (1:1) room lookup.
//!
//! A 1:1 DM is just an ordinary `direct`-kind room with exactly two members. This
//! repo owns the single "does a 1:1 already exist between these two participants?"
//! query that backs find-or-create: the HTTP layer calls [`DmRepo::find_direct`]
//! first and only falls back to creating a room (via the existing
//! [`RoomRepo`](crate::RoomRepo)) when it returns `None`.
//!
//! Purely additive: a NEW [`DmRepo`] over the EXISTING `rooms` / `room_members`
//! tables; no existing repo is touched, and room *creation* is deliberately left
//! to [`RoomRepo`](crate::RoomRepo) so the create + enroll-owner SQL lives in one
//! place. This repo carries no model struct — it returns the bare [`RoomId`] of
//! the matching DM.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;

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
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT rm1.room_id
                FROM room_members rm1
                JOIN room_members rm2 ON rm1.room_id = rm2.room_id
                JOIN rooms r ON r.id = rm1.room_id
               WHERE rm1.participant_id = $1
                 AND rm2.participant_id = $2
                 AND r.kind = 'direct'
                 AND (SELECT count(*) FROM room_members rm3 WHERE rm3.room_id = rm1.room_id) = 2
               LIMIT 1",
        )
        .bind(a.to_uuid())
        .bind(b.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| RoomId::from_uuid(id)))
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
            .max_connections(2)
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

    /// Insert a room of the given `kind` in the default workspace and enroll every
    /// `members` participant, so membership/kind/count assertions are deterministic.
    async fn mk_room(p: &PgPool, kind: &str, members: &[ParticipantId]) -> RoomId {
        let id = RoomId::new();
        let creator = members.first().copied().unwrap_or_else(ParticipantId::new);
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(kind)
        .bind(format!("dm-room-{id}"))
        .bind(creator.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
        .execute(p)
        .await
        .expect("insert room");
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
        // A non-direct (channel) room shared by dan and erin.
        let chan = mk_room(&pg, "channel", &[dan, erin]).await;
        // A 3-member "direct" room (group DM) shared by finn, gabe, hank.
        let group = mk_room(&pg, "direct", &[finn, gabe, hank]).await;

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

        // A non-direct room (channel) is never matched as a DM.
        assert_eq!(
            repo.find_direct(dan, erin).await.unwrap(),
            None,
            "a channel (non-direct) room is not matched"
        );

        // A direct room with 3 members is not a 1:1 (count != 2).
        assert_eq!(
            repo.find_direct(finn, gabe).await.unwrap(),
            None,
            "a 3-member direct room is not matched (exactly two members required)"
        );

        // Cleanup so reruns stay self-contained.
        for room in [dm, chan, group] {
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
}
