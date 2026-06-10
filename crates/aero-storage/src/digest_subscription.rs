//! Scheduled / recurring AI digest subscription repository.
//!
//! Backs `migrations/0084_digest_subscriptions.sql`. A participant subscribes to a
//! recurring AI digest of a ROOM or a WORKSPACE on a `daily` / `weekly` cadence.
//! Each row carries its next firing time in `next_run_at`; the digest dispatcher
//! polls due rows ([`due`](DigestSubscriptionRepo::due)), summarizes the target via
//! the AI service, delivers the summary, then advances `next_run_at`
//! ([`reschedule`](DigestSubscriptionRepo::reschedule)).
//!
//! Exactly one of `room_id` / `workspace_id` is set (enforced by a table CHECK and
//! by [`DigestTarget`]). Purely additive: a NEW [`DigestSubscriptionRepo`]; no
//! existing repo is touched. The [`DigestSubscription`] model lives here (and is
//! re-exported from the crate root) rather than in `aero-common`, since it is a
//! storage-layer projection. The cadence arithmetic is the pure, db-free
//! [`next_run_at`].

use aero_common::{DigestSubscriptionId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// Digest cadence: a daily digest.
pub const FREQ_DAILY: &str = "daily";
/// Digest cadence: a weekly digest.
pub const FREQ_WEEKLY: &str = "weekly";

/// Whether `frequency` is one of the recognized cadences (`daily` | `weekly`).
/// Pure, so the domain check is unit-tested offline.
#[must_use]
pub fn validate_frequency(frequency: &str) -> bool {
    matches!(frequency, FREQ_DAILY | FREQ_WEEKLY)
}

/// Advance `from` by one step of `frequency`, or `None` for an unknown cadence.
///
/// Pure and database-free, so it unit-tests without a Postgres. The recognised
/// cadences are `"daily"` (`+1d`) and `"weekly"` (`+7d`); any other string yields
/// `None`, which callers treat as a validation error.
#[must_use]
pub fn next_run_at(
    frequency: &str,
    from: time::OffsetDateTime,
) -> Option<time::OffsetDateTime> {
    let step = match frequency {
        FREQ_DAILY => time::Duration::days(1),
        FREQ_WEEKLY => time::Duration::days(7),
        _ => return None,
    };
    Some(from + step)
}

/// The target of a digest subscription — a single room XOR a whole workspace.
///
/// Mirrors the table's `(room_id IS NULL) <> (workspace_id IS NULL)` CHECK as a
/// type, so a row can never be constructed (or read back) with both or neither set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestTarget {
    /// A per-room digest — summarize this room's recent activity.
    Room(RoomId),
    /// A workspace-wide digest — summarize the caller's recent cross-channel activity.
    Workspace(WorkspaceId),
}

impl DigestTarget {
    /// The `(room_id, workspace_id)` column pair this target maps to (exactly one
    /// `Some`).
    #[must_use]
    fn columns(self) -> (Option<uuid::Uuid>, Option<uuid::Uuid>) {
        match self {
            Self::Room(r) => (Some(r.to_uuid()), None),
            Self::Workspace(w) => (None, Some(w.to_uuid())),
        }
    }

    /// Reconstruct a target from the stored column pair. Returns `None` when the
    /// XOR invariant is violated (both or neither set) — defense in depth against a
    /// row that slipped past the table CHECK.
    #[must_use]
    fn from_columns(room: Option<uuid::Uuid>, workspace: Option<uuid::Uuid>) -> Option<Self> {
        match (room, workspace) {
            (Some(r), None) => Some(Self::Room(RoomId::from_uuid(r))),
            (None, Some(w)) => Some(Self::Workspace(WorkspaceId::from_uuid(w))),
            _ => None,
        }
    }
}

/// One scheduled digest subscription.
///
/// A storage-layer projection of a `digest_subscriptions` row. `Serialize` flattens
/// the `target` into `room_id` / `workspace_id` (exactly one present) so the wire
/// shape matches the table; the timestamps render as RFC 3339.
#[derive(Debug, Clone)]
pub struct DigestSubscription {
    /// The subscription's unique id.
    pub id: DigestSubscriptionId,
    /// The participant the digest is delivered to (and owner-scoped to).
    pub participant_id: ParticipantId,
    /// What is summarized — a room XOR a workspace.
    pub target: DigestTarget,
    /// The cadence (`daily` / `weekly`).
    pub frequency: String,
    /// When the digest next fires.
    pub next_run_at: time::OffsetDateTime,
    /// When the subscription was created.
    pub created_at: time::OffsetDateTime,
}

impl Serialize for DigestSubscription {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct as _;
        let (room, workspace) = match self.target {
            DigestTarget::Room(r) => (Some(r), None),
            DigestTarget::Workspace(w) => (None, Some(w)),
        };
        let mut st = serializer.serialize_struct("DigestSubscription", 7)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("participant_id", &self.participant_id)?;
        st.serialize_field("room_id", &room)?;
        st.serialize_field("workspace_id", &workspace)?;
        st.serialize_field("frequency", &self.frequency)?;
        st.serialize_field(
            "next_run_at",
            &self
                .next_run_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        )?;
        st.serialize_field(
            "created_at",
            &self
                .created_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        )?;
        st.end()
    }
}

/// The columns a [`DigestSubscription`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, participant_id, room_id, workspace_id, frequency, next_run_at, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    Option<uuid::Uuid>,
    Option<uuid::Uuid>,
    String,
    time::OffsetDateTime,
    time::OffsetDateTime,
);

/// Decode a row into a model. Returns `None` only when the stored target violates
/// the room-XOR-workspace invariant (which the table CHECK forbids).
fn row_to_model(r: Row) -> Option<DigestSubscription> {
    let (id, participant_id, room_id, workspace_id, frequency, next_run_at, created_at) = r;
    Some(DigestSubscription {
        id: DigestSubscriptionId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        target: DigestTarget::from_columns(room_id, workspace_id)?,
        frequency,
        next_run_at,
        created_at,
    })
}

/// Repository over the `digest_subscriptions` table (scheduled AI digests).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DigestSubscriptionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DigestSubscriptionRepo {
    pool: PgPool,
}

impl DigestSubscriptionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new digest subscription, returning its generated id. The caller is
    /// responsible for membership gating and frequency validation; `first_run` is
    /// the first firing time (typically [`next_run_at`] of `now`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        target: DigestTarget,
        frequency: &str,
        first_run: time::OffsetDateTime,
    ) -> Result<DigestSubscriptionId, sqlx::Error> {
        let id = DigestSubscriptionId::new();
        let (room, workspace) = target.columns();
        sqlx::query(
            r"INSERT INTO digest_subscriptions
                  (id, participant_id, room_id, workspace_id, frequency, next_run_at)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(room)
        .bind(workspace)
        .bind(frequency)
        .bind(first_run)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a participant's own digest subscriptions, newest first. Owner-scoped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<DigestSubscription>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM digest_subscriptions
              WHERE participant_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().filter_map(row_to_model).collect())
    }

    /// Delete one of the caller's own subscriptions. Returns `true` iff a row was
    /// removed — owner-scoped (`participant_id` in the `WHERE`), so a caller can
    /// never delete another user's subscription, and a second delete (or a
    /// stranger's / unknown id) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: DigestSubscriptionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM digest_subscriptions WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the subscriptions now due (`next_run_at <= now`), soonest first. The
    /// digest dispatcher fires each, then reschedules it.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn due(
        &self,
        now: time::OffsetDateTime,
    ) -> Result<Vec<DigestSubscription>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM digest_subscriptions
              WHERE next_run_at <= $1
              ORDER BY next_run_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().filter_map(row_to_model).collect())
    }

    /// Move a subscription's next firing time to `next`. Called by the dispatcher
    /// after a firing to advance the schedule.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn reschedule(
        &self,
        id: DigestSubscriptionId,
        next: time::OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE digest_subscriptions SET next_run_at = $2 WHERE id = $1")
            .bind(id.to_uuid())
            .bind(next)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_frequency_accepts_known_cadences() {
        assert!(validate_frequency("daily"));
        assert!(validate_frequency("weekly"));
    }

    #[test]
    fn validate_frequency_rejects_unknown() {
        assert!(!validate_frequency(""));
        assert!(!validate_frequency("Daily"));
        assert!(!validate_frequency("hourly"));
        assert!(!validate_frequency("monthly"));
    }

    #[test]
    fn next_run_at_steps_by_frequency() {
        let from = time::OffsetDateTime::UNIX_EPOCH;
        assert_eq!(next_run_at("daily", from), Some(from + time::Duration::days(1)));
        assert_eq!(next_run_at("weekly", from), Some(from + time::Duration::days(7)));
        assert_eq!(next_run_at("hourly", from), None);
        assert_eq!(next_run_at("", from), None);
    }

    #[test]
    fn target_columns_are_mutually_exclusive() {
        let r = DigestTarget::Room(RoomId::new());
        let (room, ws) = r.columns();
        assert!(room.is_some() && ws.is_none());

        let w = DigestTarget::Workspace(WorkspaceId::new());
        let (room, ws) = w.columns();
        assert!(room.is_none() && ws.is_some());
    }

    #[test]
    fn target_roundtrips_through_columns() {
        let room = RoomId::new();
        let (rc, wc) = DigestTarget::Room(room).columns();
        assert_eq!(DigestTarget::from_columns(rc, wc), Some(DigestTarget::Room(room)));

        let ws = WorkspaceId::new();
        let (rc, wc) = DigestTarget::Workspace(ws).columns();
        assert_eq!(
            DigestTarget::from_columns(rc, wc),
            Some(DigestTarget::Workspace(ws))
        );
    }

    #[test]
    fn target_from_columns_rejects_both_or_neither() {
        assert_eq!(DigestTarget::from_columns(None, None), None);
        assert_eq!(
            DigestTarget::from_columns(Some(uuid::Uuid::nil()), Some(uuid::Uuid::nil())),
            None
        );
    }

    #[test]
    fn serialize_flattens_target_into_room_xor_workspace() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let room = RoomId::new();
        let sub = DigestSubscription {
            id: DigestSubscriptionId::new(),
            participant_id: ParticipantId::new(),
            target: DigestTarget::Room(room),
            frequency: "daily".into(),
            next_run_at: now,
            created_at: now,
        };
        let v = serde_json::to_value(&sub).expect("serialize");
        assert_eq!(v["room_id"], serde_json::json!(room));
        assert!(v["workspace_id"].is_null(), "workspace_id absent for a room digest");
        assert_eq!(v["frequency"], "daily");
        assert_eq!(v["next_run_at"], "1970-01-01T00:00:00Z");

        // Workspace target: the mirror — workspace_id set, room_id null.
        let ws = WorkspaceId::new();
        let sub_ws = DigestSubscription {
            target: DigestTarget::Workspace(ws),
            ..sub
        };
        let vw = serde_json::to_value(&sub_ws).expect("serialize");
        assert_eq!(vw["workspace_id"], serde_json::json!(ws));
        assert!(vw["room_id"].is_null(), "room_id absent for a workspace digest");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored digest_subscription
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("digest-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn digest_create_due_reschedule_delete() {
        let p = pool();
        let repo = DigestSubscriptionRepo::new(p.clone());
        let owner = participant(&p).await;
        let room = RoomId::new();

        // Create a room digest whose first run is already in the past → due now.
        let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        let id = repo
            .create(owner, DigestTarget::Room(room), "daily", past)
            .await
            .unwrap();

        // list_for shows it (owner-scoped); a stranger's list does not.
        let listed = repo.list_for(owner).await.unwrap();
        assert!(listed.iter().any(|s| s.id == id), "owner list shows it");
        assert_eq!(
            listed.iter().find(|s| s.id == id).unwrap().target,
            DigestTarget::Room(room)
        );
        let stranger = participant(&p).await;
        assert!(
            !repo.list_for(stranger).await.unwrap().iter().any(|s| s.id == id),
            "another user's list does not show it"
        );

        // due() returns it (past next_run_at).
        let now = time::OffsetDateTime::now_utc();
        assert!(repo.due(now).await.unwrap().iter().any(|s| s.id == id), "past run is due");

        // reschedule into the future ⇒ no longer due.
        let next = next_run_at("daily", now).expect("known cadence");
        repo.reschedule(id, next).await.unwrap();
        assert!(
            !repo
                .due(time::OffsetDateTime::now_utc())
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == id),
            "not due after reschedule into the future"
        );

        // delete: owner-scoped; stranger cannot, owner can, second is a no-op.
        assert!(!repo.delete(id, stranger).await.unwrap(), "stranger cannot delete");
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(!repo.delete(id, owner).await.unwrap(), "second delete is a no-op");
        assert!(
            !repo.list_for(owner).await.unwrap().iter().any(|s| s.id == id),
            "deleted subscription leaves the list"
        );

        // Cleanup participants.
        for who in [owner, stranger] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn digest_workspace_target_persists() {
        let p = pool();
        let repo = DigestSubscriptionRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws = WorkspaceId::new();

        let first = time::OffsetDateTime::now_utc() + time::Duration::days(1);
        let id = repo
            .create(owner, DigestTarget::Workspace(ws), "weekly", first)
            .await
            .unwrap();
        let listed = repo.list_for(owner).await.unwrap();
        let found = listed.iter().find(|s| s.id == id).expect("present");
        assert_eq!(found.target, DigestTarget::Workspace(ws));
        assert_eq!(found.frequency, "weekly");

        repo.delete(id, owner).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
