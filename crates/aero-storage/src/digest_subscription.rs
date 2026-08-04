//! Scheduled / recurring AI digest subscription repository.
//!
//! Backs `migrations/0084_digest_subscriptions.sql`. A participant subscribes to a
//! recurring AI digest of a ROOM or a WORKSPACE on a `daily` / `weekly` cadence.
//! Each row carries its next firing time in `next_run_at`; the digest dispatcher
//! leases one occurrence, durably prepares its summary, delivers it idempotently,
//! and advances `next_run_at` only after a fenced success confirmation. Failed
//! generation/delivery is re-parked without losing the occurrence.
//!
//! Exactly one of `room_id` / `workspace_id` is set (enforced by a table CHECK and
//! by [`DigestTarget`]). Purely additive: a NEW [`DigestSubscriptionRepo`]; no
//! existing repo is touched. The [`DigestSubscription`] model lives here (and is
//! re-exported from the crate root) rather than in `aero-common`, since it is a
//! storage-layer projection. The cadence arithmetic is the pure, db-free
//! [`next_run_at`].

use aero_common::{ActivityId, DigestSubscriptionId, ParticipantId, RoomId, WorkspaceId};
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
pub fn next_run_at(frequency: &str, from: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
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
    /// Attempts made for the currently due occurrence.
    pub delivery_attempts: i32,
    /// Earliest retry time after a transient failure.
    pub retry_at: Option<time::OffsetDateTime>,
    /// Last delivery failure, retained for owner visibility.
    pub last_error: Option<String>,
    /// Terminal failure timestamp. Dead subscriptions remain listable until
    /// their owner deletes them.
    pub dead_at: Option<time::OffsetDateTime>,
}

/// One leased digest occurrence.
#[derive(Debug, Clone)]
pub struct DigestDeliveryClaim {
    pub subscription: DigestSubscription,
    pub delivery_key: uuid::Uuid,
    pub claim_token: uuid::Uuid,
    pub attempt: i32,
    pub prepared_summary: Option<String>,
    pub lease_expires_at: time::OffsetDateTime,
}

/// Result of a token-fenced digest failure settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFailureDisposition {
    RetryScheduled,
    Dead,
    FenceLost,
}

/// Outcome of the authorization-checked workspace feed insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDigestDelivery {
    Inserted,
    AlreadyDelivered,
    AccessRevoked,
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
        let mut st = serializer.serialize_struct("DigestSubscription", 11)?;
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
        st.serialize_field("delivery_attempts", &self.delivery_attempts)?;
        st.serialize_field(
            "retry_at",
            &self.retry_at.map(|retry_at| {
                retry_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            }),
        )?;
        st.serialize_field("last_error", &self.last_error)?;
        st.serialize_field(
            "dead_at",
            &self.dead_at.map(|dead_at| {
                dead_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            }),
        )?;
        st.end()
    }
}

/// The columns a [`DigestSubscription`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, room_id, workspace_id, frequency, next_run_at,
    created_at, delivery_attempts, retry_at, last_error, dead_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    Option<uuid::Uuid>,
    Option<uuid::Uuid>,
    String,
    time::OffsetDateTime,
    time::OffsetDateTime,
    i32,
    Option<time::OffsetDateTime>,
    Option<String>,
    Option<time::OffsetDateTime>,
);

#[derive(Debug, sqlx::FromRow)]
struct ClaimRow {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    room_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    frequency: String,
    next_run_at: time::OffsetDateTime,
    created_at: time::OffsetDateTime,
    retry_at: Option<time::OffsetDateTime>,
    last_error: Option<String>,
    dead_at: Option<time::OffsetDateTime>,
    delivery_key: uuid::Uuid,
    claim_token: uuid::Uuid,
    delivery_attempts: i32,
    prepared_summary: Option<String>,
    lease_expires_at: time::OffsetDateTime,
}

/// Decode a row into a model. Returns `None` only when the stored target violates
/// the room-XOR-workspace invariant (which the table CHECK forbids).
fn row_to_model(r: Row) -> Option<DigestSubscription> {
    let (
        id,
        participant_id,
        room_id,
        workspace_id,
        frequency,
        next_run_at,
        created_at,
        delivery_attempts,
        retry_at,
        last_error,
        dead_at,
    ) = r;
    Some(DigestSubscription {
        id: DigestSubscriptionId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        target: DigestTarget::from_columns(room_id, workspace_id)?,
        frequency,
        next_run_at,
        created_at,
        delivery_attempts,
        retry_at,
        last_error,
        dead_at,
    })
}

fn claim_row_to_model(row: ClaimRow) -> Option<DigestDeliveryClaim> {
    Some(DigestDeliveryClaim {
        subscription: DigestSubscription {
            id: DigestSubscriptionId::from_uuid(row.id),
            participant_id: ParticipantId::from_uuid(row.participant_id),
            target: DigestTarget::from_columns(row.room_id, row.workspace_id)?,
            frequency: row.frequency,
            next_run_at: row.next_run_at,
            created_at: row.created_at,
            delivery_attempts: row.delivery_attempts,
            retry_at: row.retry_at,
            last_error: row.last_error,
            dead_at: row.dead_at,
        },
        delivery_key: row.delivery_key,
        claim_token: row.claim_token,
        attempt: row.delivery_attempts,
        prepared_summary: row.prepared_summary,
        lease_expires_at: row.lease_expires_at,
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
    /// removed — owner-scoped (`participant_id` in the `WHERE`) and allowed only
    /// while no worker owns a lease. Re-parked failures are safe to delete; an
    /// actively claimed occurrence remains fenced. A second delete (or a
    /// stranger's / unknown id) returns `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: DigestSubscriptionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM digest_subscriptions
              WHERE id = $1
                AND participant_id = $2
                AND claim_token IS NULL",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Lease due subscriptions across replicas using `SKIP LOCKED` and the
    /// default retry bound.
    pub async fn claim_due(
        &self,
        now: time::OffsetDateTime,
        lease: time::Duration,
        limit: i64,
    ) -> Result<Vec<DigestDeliveryClaim>, sqlx::Error> {
        self.claim_due_with_policy(now, lease, 8, limit).await
    }

    /// Lease due subscriptions and terminalize expired claims that exhausted
    /// their bounded attempt budget.
    pub async fn claim_due_with_policy(
        &self,
        now: time::OffsetDateTime,
        lease: time::Duration,
        max_attempts: i32,
        limit: i64,
    ) -> Result<Vec<DigestDeliveryClaim>, sqlx::Error> {
        let lease_secs = lease.whole_seconds().clamp(1, 3600);
        let max_attempts = max_attempts.clamp(1, 100);
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, ClaimRow>(
            "WITH exhausted_ids AS (
                 SELECT id
                   FROM digest_subscriptions
                  WHERE dead_at IS NULL
                    AND claim_token IS NOT NULL
                    AND lease_expires_at <= $1
                    AND delivery_attempts >= $4
                  ORDER BY lease_expires_at, id
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             ),
             exhausted AS (
                 UPDATE digest_subscriptions AS digest
                    SET dead_at = $1,
                        last_error = COALESCE(
                            digest.last_error,
                            'delivery lease expired after maximum attempts'
                        ),
                        claim_token = NULL,
                        claimed_at = NULL,
                        lease_expires_at = NULL,
                        delivery_key = NULL,
                        retry_at = NULL,
                        prepared_summary = NULL
                   FROM exhausted_ids
                  WHERE digest.id = exhausted_ids.id
             ),
             due AS (
                 SELECT id
                   FROM digest_subscriptions
                  WHERE dead_at IS NULL
                    AND next_run_at <= $1
                    AND COALESCE(retry_at, next_run_at) <= $1
                    AND delivery_attempts < $4
                    AND (
                        claim_token IS NULL
                        OR lease_expires_at <= $1
                    )
                  ORDER BY COALESCE(retry_at, next_run_at), next_run_at, id
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             )
             UPDATE digest_subscriptions AS digest
                SET claim_token = gen_random_uuid(),
                    claimed_at = $1,
                    lease_expires_at =
                        $1 + make_interval(secs => $3::double precision),
                    delivery_key = COALESCE(digest.delivery_key, gen_random_uuid()),
                    delivery_attempts = digest.delivery_attempts + 1
               FROM due
              WHERE digest.id = due.id
          RETURNING digest.id,
                    digest.participant_id,
                    digest.room_id,
                    digest.workspace_id,
                    digest.frequency,
                    digest.next_run_at,
                    digest.created_at,
                    digest.retry_at,
                    digest.last_error,
                    digest.dead_at,
                    digest.delivery_key,
                    digest.claim_token,
                    digest.delivery_attempts,
                    digest.prepared_summary,
                    digest.lease_expires_at",
        )
        .bind(now)
        .bind(limit)
        .bind(lease_secs)
        .bind(max_attempts)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(claim_row_to_model).collect())
    }

    /// Persist the generated summary before any delivery side effect.
    pub async fn save_prepared_summary(
        &self,
        id: DigestSubscriptionId,
        claim_token: uuid::Uuid,
        delivery_key: uuid::Uuid,
        attempt: i32,
        summary: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE digest_subscriptions
                SET prepared_summary = $5
              WHERE id = $1
                AND dead_at IS NULL
                AND claim_token = $2
                AND delivery_key = $3
                AND delivery_attempts = $4",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(delivery_key)
        .bind(attempt)
        .bind(summary)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Insert a workspace digest activity idempotently for this occurrence.
    pub async fn insert_workspace_delivery(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        delivery_key: uuid::Uuid,
        summary: &str,
    ) -> Result<WorkspaceDigestDelivery, sqlx::Error> {
        let (allowed, inserted): (bool, bool) = sqlx::query_as(
            "WITH access AS MATERIALIZED (
                 SELECT aero_effective_workspace_access($5, $2) AS allowed
             ),
             inserted AS (
                 INSERT INTO activity_feed
                      (id, participant_id, kind, actor_id, subject_id, summary)
                 SELECT $1, $2, 'digest', NULL, $3, $4
                   FROM access
                  WHERE access.allowed
                 ON CONFLICT (participant_id, subject_id)
                     WHERE kind = 'digest' AND subject_id IS NOT NULL
                 DO NOTHING
                 RETURNING id
             )
             SELECT access.allowed, EXISTS(SELECT 1 FROM inserted)
               FROM access",
        )
        .bind(ActivityId::new().to_uuid())
        .bind(participant.to_uuid())
        .bind(delivery_key)
        .bind(summary)
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(match (allowed, inserted) {
            (false, _) => WorkspaceDigestDelivery::AccessRevoked,
            (true, true) => WorkspaceDigestDelivery::Inserted,
            (true, false) => WorkspaceDigestDelivery::AlreadyDelivered,
        })
    }

    /// Advance only after generation and delivery both succeeded.
    #[allow(clippy::too_many_arguments)]
    pub async fn confirm_sent(
        &self,
        id: DigestSubscriptionId,
        claim_token: uuid::Uuid,
        delivery_key: uuid::Uuid,
        attempt: i32,
        next_run: time::OffsetDateTime,
        sent_at: time::OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE digest_subscriptions
                SET next_run_at = $5,
                    last_sent_at = $6,
                    claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL,
                    delivery_key = NULL,
                    delivery_attempts = 0,
                    retry_at = NULL,
                    last_error = NULL,
                    prepared_summary = NULL,
                    dead_at = NULL
              WHERE id = $1
                AND dead_at IS NULL
                AND claim_token = $2
                AND delivery_key = $3
                AND delivery_attempts = $4",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(delivery_key)
        .bind(attempt)
        .bind(next_run)
        .bind(sent_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Re-park a transient failure without advancing its cadence cursor, or
    /// retain a visible terminal failure after permanent/exhausted delivery.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_failure(
        &self,
        id: DigestSubscriptionId,
        claim_token: uuid::Uuid,
        delivery_key: uuid::Uuid,
        attempt: i32,
        retry_at: time::OffsetDateTime,
        error: &str,
        retryable: bool,
        max_attempts: i32,
    ) -> Result<DigestFailureDisposition, sqlx::Error> {
        let max_attempts = max_attempts.clamp(1, 100);
        let dead_at: Option<(Option<time::OffsetDateTime>,)> = sqlx::query_as(
            "UPDATE digest_subscriptions
                SET claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL,
                    retry_at = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN $5
                        ELSE NULL
                    END,
                    last_error = left($6, 2048),
                    dead_at = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN NULL
                        ELSE now()
                    END,
                    delivery_key = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN delivery_key
                        ELSE NULL
                    END,
                    prepared_summary = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN prepared_summary
                        ELSE NULL
                    END
              WHERE id = $1
                AND dead_at IS NULL
                AND claim_token = $2
                AND delivery_key = $3
                AND delivery_attempts = $4
          RETURNING dead_at",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(delivery_key)
        .bind(attempt)
        .bind(retry_at)
        .bind(error)
        .bind(retryable)
        .bind(max_attempts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match dead_at {
            Some((None,)) => DigestFailureDisposition::RetryScheduled,
            Some((Some(_),)) => DigestFailureDisposition::Dead,
            None => DigestFailureDisposition::FenceLost,
        })
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
        assert_eq!(
            next_run_at("daily", from),
            Some(from + time::Duration::days(1))
        );
        assert_eq!(
            next_run_at("weekly", from),
            Some(from + time::Duration::days(7))
        );
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
        assert_eq!(
            DigestTarget::from_columns(rc, wc),
            Some(DigestTarget::Room(room))
        );

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
            delivery_attempts: 0,
            retry_at: None,
            last_error: None,
            dead_at: None,
        };
        let v = serde_json::to_value(&sub).expect("serialize");
        assert_eq!(v["room_id"], serde_json::json!(room));
        assert!(
            v["workspace_id"].is_null(),
            "workspace_id absent for a room digest"
        );
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
        assert!(
            vw["room_id"].is_null(),
            "room_id absent for a workspace digest"
        );
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored digest_subscription
/// ```
#[cfg(test)]
#[path = "digest_subscription/db_tests.rs"]
mod db_tests;
