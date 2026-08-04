//! Server-issued search impressions and click-feedback.
//!
//! Advanced search persists the exact ordered result ids it returned in a
//! short-lived [`SearchImpression`]. A click consumes that proof inside one
//! transaction: the repository derives the rank and query from the snapshot,
//! rechecks canonical workspace/current effective room access, and writes at
//! most one analytics event per impression. Clients never supply relevance
//! attributes.
//!
//! Legacy rows from `migrations/0133_search_click_events.sql` remain readable.
//! Migration 0221 adds the proof tables/columns and validates every
//! impression-backed write without rewriting old click history.

use std::collections::BTreeSet;

use aero_common::{Error, MessageId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// Maximum accepted advanced-search query length, measured in Unicode scalar
/// values to match `PostgreSQL` `char_length`.
pub const MAX_SEARCH_QUERY_CHARS: usize = 2_000;

/// Maximum number of ordered results carried by one impression.
pub const MAX_IMPRESSION_RESULTS: usize = 100;

/// Normalize a client search query before parsing or persistence.
///
/// All Unicode whitespace runs become one ASCII space. Empty and overlong
/// inputs are rejected before search work starts.
///
/// # Errors
/// Returns [`Error::Invalid`] for empty or overlong input.
pub fn normalize_search_query(raw: &str) -> Result<String, Error> {
    if raw.chars().count() > MAX_SEARCH_QUERY_CHARS {
        return Err(Error::Invalid(format!(
            "query exceeds {MAX_SEARCH_QUERY_CHARS} characters"
        )));
    }
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(Error::Invalid("empty query".into()));
    }
    if normalized.chars().count() > MAX_SEARCH_QUERY_CHARS {
        return Err(Error::Invalid(format!(
            "query exceeds {MAX_SEARCH_QUERY_CHARS} characters"
        )));
    }
    Ok(normalized)
}

fn validate_feedback_query(query: &str) -> Result<(), Error> {
    if query.is_empty() {
        return Err(Error::Invalid("empty query".into()));
    }
    if query.chars().count() > MAX_SEARCH_QUERY_CHARS {
        return Err(Error::Invalid(format!(
            "query exceeds {MAX_SEARCH_QUERY_CHARS} characters"
        )));
    }
    if query.split_whitespace().collect::<Vec<_>>().join(" ") != query {
        return Err(Error::Invalid("query must be normalized".into()));
    }
    Ok(())
}

/// A short-lived proof of the exact search results returned to one participant.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SearchImpression {
    /// Opaque proof id returned to the client.
    pub id: uuid::Uuid,
    /// Last instant at which a click may consume this proof.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: time::OffsetDateTime,
}

/// Stable receipt returned for both a first click and an identical retry.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SearchClickReceipt {
    /// The consumed proof.
    pub impression_id: uuid::Uuid,
    /// Result selected from the proof's ordered snapshot.
    pub result_id: MessageId,
    /// Server-derived zero-based position in that snapshot.
    pub result_rank: i32,
}

/// Aggregate relevance signal over a window of recorded search clicks.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CtrStats {
    /// Number of proof-backed result-list impressions in the window.
    ///
    /// Legacy click rows remain in the click/MRR aggregates but have no
    /// trustworthy impression denominator, so they are intentionally excluded
    /// here.
    pub impressions: i64,
    /// Number of recorded click-throughs in the window.
    pub clicks: i64,
    /// Number of distinct normalized queries with at least one click.
    pub queries: i64,
    /// Mean reciprocal rank over valid zero-based ranks.
    pub mean_reciprocal_rank: f64,
    /// Fraction of proof-backed impressions that produced any click.
    pub click_through_rate: f64,
    /// Fraction of proof-backed impressions whose rank-zero result was clicked.
    pub top_result_ctr: f64,
}

#[derive(sqlx::FromRow)]
struct ResultScopeRow {
    room_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    is_live: bool,
}

#[derive(Clone, sqlx::FromRow)]
struct ImpressionRow {
    workspace_id: uuid::Uuid,
    query_text: String,
    result_ids: Vec<uuid::Uuid>,
    created_at: time::OffsetDateTime,
    expires_at: time::OffsetDateTime,
    unexpired: bool,
    clicked_result_id: Option<uuid::Uuid>,
}

/// Repository over `search_impressions` and `search_click_events`.
#[derive(Clone)]
#[must_use]
pub struct SearchFeedbackRepo {
    pool: PgPool,
}

impl SearchFeedbackRepo {
    /// Build a repository over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist the exact ordered results of a successful advanced search.
    ///
    /// The transaction fences current workspace and room access, validates every
    /// result's canonical workspace/live state, and then inserts the 15-minute
    /// proof. Empty result snapshots are valid and still receive an impression.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an invalid query/snapshot,
    /// [`Error::Forbidden`] when effective access has been revoked, or a database
    /// error.
    pub async fn create_impression(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query_text: &str,
        results: &[MessageId],
    ) -> Result<SearchImpression, Error> {
        validate_feedback_query(query_text)?;
        if results.len() > MAX_IMPRESSION_RESULTS {
            return Err(Error::Invalid(format!(
                "search impression exceeds {MAX_IMPRESSION_RESULTS} results"
            )));
        }

        let result_ids = results.iter().map(MessageId::to_uuid).collect::<Vec<_>>();
        if result_ids.iter().copied().collect::<BTreeSet<_>>().len() != result_ids.len() {
            return Err(Error::Invalid(
                "search impression contains duplicate results".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        assert_effective_workspace_access(&mut tx, workspace, participant).await?;

        if !result_ids.is_empty() {
            let scope = resolve_result_scope(&mut tx, &result_ids).await?;
            if scope.len() != result_ids.len()
                || scope.iter().any(|row| {
                    !row.is_live
                        || row.workspace_id != Some(workspace.to_uuid())
                        || row.room_id.is_none()
                })
            {
                return Err(Error::NotFound("search result not found".into()));
            }

            let rooms = scope
                .iter()
                .filter_map(|row| row.room_id)
                .collect::<BTreeSet<_>>();
            for room in rooms {
                assert_effective_room_access(
                    &mut tx,
                    RoomId::from_uuid(room),
                    workspace,
                    participant,
                )
                .await?;
            }
            lock_live_results(&mut tx, workspace, &result_ids).await?;
        }

        let (id, expires_at): (uuid::Uuid, time::OffsetDateTime) = sqlx::query_as(
            r"INSERT INTO search_impressions
                 (participant_id, workspace_id, query_text, result_ids)
               VALUES ($1, $2, $3, $4)
               RETURNING id, expires_at",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(query_text)
        .bind(&result_ids)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SearchImpression { id, expires_at })
    }

    /// Consume an impression and record one click.
    ///
    /// `query_text` and rank are derived exclusively from the locked proof.
    /// Canonical result workspace/live state and current effective room access
    /// are rechecked immediately before mutation. Repeating the same
    /// `(impression, result)` returns the same receipt without a second event;
    /// selecting a different result after consumption is a conflict.
    ///
    /// # Errors
    /// Rejects missing/wrong-owner proofs, expired proofs, non-snapshot results,
    /// revoked access, canonical-scope drift, and conflicting second clicks.
    pub async fn record_impression_click(
        &self,
        participant: ParticipantId,
        impression_id: uuid::Uuid,
        result: MessageId,
    ) -> Result<SearchClickReceipt, Error> {
        let mut tx = self.pool.begin().await?;
        // Resolve the proof without a row lock so the transaction can enter the
        // global workspace -> room -> membership -> message order first. Locking
        // the impression here would invert workspace deletion's
        // workspace -> child-cascade order.
        let resolved = load_impression(&mut tx, impression_id, participant, false)
            .await?
            .ok_or_else(|| Error::NotFound("search impression not found".into()))?;
        let result_uuid = result.to_uuid();
        let resolved_rank = resolved
            .result_ids
            .iter()
            .position(|candidate| *candidate == result_uuid)
            .ok_or_else(|| Error::Invalid("result is not part of the impression".into()))
            .and_then(|position| {
                i32::try_from(position)
                    .map_err(|_| Error::Invalid("search result rank is out of range".into()))
            })?;
        let workspace = WorkspaceId::from_uuid(resolved.workspace_id);

        // Even an already-committed retry enters the canonical workspace row
        // before the impression lock. It does not re-require membership: a
        // stable receipt must survive later revocation/deletion/expiry.
        lock_workspace_boundary(&mut tx, workspace).await?;

        if resolved.clicked_result_id.is_none() {
            let precondition = match resolve_live_result_room(&mut tx, result, workspace).await {
                Ok(room) => {
                    match assert_effective_room_access(&mut tx, room, workspace, participant).await
                    {
                        Ok(()) => lock_live_result(&mut tx, result, room, workspace).await,
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };

            if let Err(error) = precondition {
                match error {
                    error @ (Error::NotFound(_) | Error::Forbidden(_)) => {
                        // A concurrent identical request may have committed
                        // before access or message state changed. Recheck the
                        // serialized proof before reporting the stale failure.
                        let locked = lock_impression(&mut tx, impression_id, participant).await?;
                        ensure_same_impression(&resolved, &locked)?;
                        if locked.clicked_result_id == Some(result_uuid) {
                            let receipt = load_click_receipt(
                                &mut tx,
                                impression_id,
                                participant,
                                workspace,
                                &locked.query_text,
                                result,
                                resolved_rank,
                            )
                            .await?;
                            tx.commit().await?;
                            return Ok(receipt);
                        }
                        if locked.clicked_result_id.is_some() {
                            return Err(Error::Conflict(
                                "search impression already consumed by another result".into(),
                            ));
                        }
                        return Err(error);
                    }
                    error => return Err(error),
                }
            }
        }

        let impression = lock_impression(&mut tx, impression_id, participant).await?;
        ensure_same_impression(&resolved, &impression)?;
        let rank = impression
            .result_ids
            .iter()
            .position(|candidate| *candidate == result_uuid)
            .and_then(|position| i32::try_from(position).ok())
            .ok_or_else(|| Error::Conflict("search impression proof changed".into()))?;

        if let Some(clicked) = impression.clicked_result_id {
            if clicked != result_uuid {
                return Err(Error::Conflict(
                    "search impression already consumed by another result".into(),
                ));
            }
            let receipt = load_click_receipt(
                &mut tx,
                impression_id,
                participant,
                workspace,
                &impression.query_text,
                result,
                rank,
            )
            .await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if !impression.unexpired {
            return Err(Error::Conflict("search impression expired".into()));
        }

        let update = sqlx::query(
            r"UPDATE search_impressions
                 SET clicked_result_id = $2,
                     clicked_at = clock_timestamp()
               WHERE id = $1
                 AND clicked_result_id IS NULL
                 AND expires_at > clock_timestamp()",
        )
        .bind(impression_id)
        .bind(result_uuid)
        .execute(&mut *tx)
        .await;
        let updated = match update {
            Ok(done) => done.rows_affected(),
            Err(error)
                if error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint)
                    == Some("search_impressions_click_proof_chk") =>
            {
                // The trigger owns the authoritative consume timestamp. Its
                // check can cross the expiry boundary a few instructions after
                // the UPDATE predicate; expose that as the same stable conflict
                // as every other expired first attempt, never as HTTP 500.
                return Err(Error::Conflict("search impression expired".into()));
            }
            Err(error) => return Err(error.into()),
        };
        if updated != 1 {
            return Err(Error::Conflict("search impression expired".into()));
        }

        sqlx::query(
            r"INSERT INTO search_click_events
                 (participant_id, workspace_id, query_text, result_id,
                  result_rank, clicked_at, impression_id)
               SELECT participant_id, workspace_id, query_text, $2,
                      $3, clicked_at, id
                 FROM search_impressions
                WHERE id = $1
               ON CONFLICT (impression_id)
                 WHERE impression_id IS NOT NULL
               DO NOTHING",
        )
        .bind(impression_id)
        .bind(result_uuid)
        .bind(rank)
        .execute(&mut *tx)
        .await?;

        let receipt = load_click_receipt(
            &mut tx,
            impression_id,
            participant,
            workspace,
            &impression.query_text,
            result,
            rank,
        )
        .await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Aggregate impressions and clicks over the last `window_days`.
    ///
    /// The reciprocal-rank expression widens the legacy `INTEGER` rank to
    /// `BIGINT` and then `NUMERIC`, avoiding `i32::MAX + 1` overflow. Historical
    /// out-of-range rows are excluded from MRR while new rows are constrained to
    /// `0..=99`. Click count/MRR retain rolling pre-0221 rows; CTR denominators
    /// use only proof-backed impressions because legacy rows never recorded a
    /// trustworthy impression.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn ctr_stats(
        &self,
        workspace: WorkspaceId,
        window_days: i32,
    ) -> Result<CtrStats, sqlx::Error> {
        let (impressions, clicks, queries, mrr, ctr, top): (i64, i64, i64, f64, f64, f64) =
            sqlx::query_as(
                r"WITH click_rollup AS (
                     SELECT COUNT(*)::bigint AS clicks,
                            COUNT(DISTINCT query_text)::bigint AS queries,
                            COALESCE(
                              AVG(
                                CASE
                                  WHEN result_rank BETWEEN 0 AND 99
                                  THEN 1.0::numeric
                                       / (result_rank::bigint + 1)::numeric
                                  ELSE NULL
                                END
                              ),
                              0
                            )::double precision AS mrr
                       FROM search_click_events
                      WHERE workspace_id = $1
                        AND clicked_at
                            > now() - make_interval(days => $2)
                   ),
                   impression_rollup AS (
                     SELECT COUNT(*)::bigint AS impressions,
                            COALESCE(
                              AVG((clicked_result_id IS NOT NULL)::int),
                              0
                            )::double precision AS ctr,
                            COALESCE(
                              AVG((
                                clicked_result_id IS NOT NULL
                                AND clicked_result_id = result_ids[1]
                              )::int),
                              0
                            )::double precision AS top_ctr
                       FROM search_impressions
                      WHERE workspace_id = $1
                        AND created_at
                            > now() - make_interval(days => $2)
                   )
                   SELECT impression_rollup.impressions,
                          click_rollup.clicks,
                          click_rollup.queries,
                          click_rollup.mrr,
                          impression_rollup.ctr,
                          impression_rollup.top_ctr
                     FROM click_rollup
                     CROSS JOIN impression_rollup",
            )
            .bind(workspace.to_uuid())
            .bind(window_days.max(1))
            .fetch_one(&self.pool)
            .await?;
        Ok(CtrStats {
            impressions,
            clicks,
            queries,
            mean_reciprocal_rank: mrr,
            click_through_rate: ctr,
            top_result_ctr: top,
        })
    }

    /// Delete click history and impression analytics older than `cutoff`.
    ///
    /// `expires_at` ends a proof's 15-minute mutation window; it does not end
    /// the analytics lifetime. Retaining expired impressions until the same
    /// configured cutoff as clicks preserves the denominator required for real
    /// click-through rates. Deleting an old impression clears a retained click's
    /// nullable `impression_id` through its foreign key.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn sweep_before(&self, cutoff: time::OffsetDateTime) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let clicks = sqlx::query("DELETE FROM search_click_events WHERE clicked_at < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let impressions = sqlx::query("DELETE FROM search_impressions WHERE created_at < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(clicks + impressions)
    }

    /// Legacy compatibility helper used only by migration/aggregate tests.
    #[cfg(test)]
    async fn record_legacy_click(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query_text: &str,
        result: MessageId,
        rank: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO search_click_events
                 (participant_id, workspace_id, query_text, result_id, result_rank)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(query_text)
        .bind(result.to_uuid())
        .bind(rank.clamp(0, 99))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

async fn load_impression(
    tx: &mut Transaction<'_, Postgres>,
    impression_id: uuid::Uuid,
    participant: ParticipantId,
    for_update: bool,
) -> Result<Option<ImpressionRow>, sqlx::Error> {
    let sql = if for_update {
        r"SELECT workspace_id, query_text, result_ids, created_at, expires_at,
                 expires_at > clock_timestamp() AS unexpired,
                 clicked_result_id
            FROM search_impressions
           WHERE id = $1
             AND participant_id = $2
           FOR UPDATE"
    } else {
        r"SELECT workspace_id, query_text, result_ids, created_at, expires_at,
                 expires_at > clock_timestamp() AS unexpired,
                 clicked_result_id
            FROM search_impressions
           WHERE id = $1
             AND participant_id = $2"
    };
    sqlx::query_as::<_, ImpressionRow>(sql)
        .bind(impression_id)
        .bind(participant.to_uuid())
        .fetch_optional(&mut **tx)
        .await
}

async fn lock_impression(
    tx: &mut Transaction<'_, Postgres>,
    impression_id: uuid::Uuid,
    participant: ParticipantId,
) -> Result<ImpressionRow, Error> {
    load_impression(tx, impression_id, participant, true)
        .await?
        .ok_or_else(|| Error::NotFound("search impression not found".into()))
}

fn ensure_same_impression(resolved: &ImpressionRow, locked: &ImpressionRow) -> Result<(), Error> {
    if resolved.workspace_id != locked.workspace_id
        || resolved.query_text != locked.query_text
        || resolved.result_ids != locked.result_ids
        || resolved.created_at != locked.created_at
        || resolved.expires_at != locked.expires_at
    {
        return Err(Error::Conflict(
            "search impression proof changed during authorization".into(),
        ));
    }
    Ok(())
}

async fn lock_workspace_boundary(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), Error> {
    let exists =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound(
            "search impression workspace not found".into(),
        ))
    }
}

async fn load_click_receipt(
    tx: &mut Transaction<'_, Postgres>,
    impression_id: uuid::Uuid,
    participant: ParticipantId,
    workspace: WorkspaceId,
    query_text: &str,
    result: MessageId,
    rank: i32,
) -> Result<SearchClickReceipt, Error> {
    let recorded = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, uuid::Uuid, i32)>(
        r"SELECT participant_id, workspace_id, query_text, result_id, result_rank
            FROM search_click_events
           WHERE impression_id = $1",
    )
    .bind(impression_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Conflict("consumed search impression has no click receipt".into()))?;
    if recorded.0 != participant.to_uuid()
        || recorded.1 != workspace.to_uuid()
        || recorded.2 != query_text
        || recorded.3 != result.to_uuid()
        || recorded.4 != rank
    {
        return Err(Error::Conflict(
            "search impression already consumed by another result".into(),
        ));
    }
    Ok(SearchClickReceipt {
        impression_id,
        result_id: result,
        result_rank: rank,
    })
}

async fn assert_effective_workspace_access(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_workspace_access($1, $2)")
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Forbidden("workspace access denied".into()))
    }
}

async fn assert_effective_room_access(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Forbidden("search result access denied".into()))
    }
}

async fn resolve_result_scope(
    tx: &mut Transaction<'_, Postgres>,
    results: &[uuid::Uuid],
) -> Result<Vec<ResultScopeRow>, sqlx::Error> {
    sqlx::query_as(
        r"SELECT message.room_id,
                 room.workspace_id,
                 (
                   message.id IS NOT NULL
                   AND message.deleted_at IS NULL
                   AND (
                     message.expires_at IS NULL
                     OR message.expires_at > clock_timestamp()
                   )
                 ) AS is_live
            FROM unnest($1::uuid[]) WITH ORDINALITY
                 AS listed(result_id, ordinal)
            LEFT JOIN messages message ON message.id = listed.result_id
            LEFT JOIN rooms room ON room.id = message.room_id
           ORDER BY listed.ordinal",
    )
    .bind(results)
    .fetch_all(&mut **tx)
    .await
}

async fn lock_live_results(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    results: &[uuid::Uuid],
) -> Result<(), Error> {
    let locked = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT message.id
            FROM messages message
            JOIN rooms room ON room.id = message.room_id
           WHERE message.id = ANY($1)
             AND room.workspace_id = $2
             AND message.deleted_at IS NULL
             AND (
               message.expires_at IS NULL
               OR message.expires_at > clock_timestamp()
             )
           ORDER BY message.id
           FOR SHARE OF message, room",
    )
    .bind(results)
    .bind(workspace.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    if locked.len() == results.len() {
        Ok(())
    } else {
        Err(Error::NotFound("search result not found".into()))
    }
}

async fn resolve_live_result_room(
    tx: &mut Transaction<'_, Postgres>,
    result: MessageId,
    workspace: WorkspaceId,
) -> Result<RoomId, Error> {
    let room = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT message.room_id
            FROM messages message
            JOIN rooms room ON room.id = message.room_id
           WHERE message.id = $1
             AND room.workspace_id = $2
             AND message.deleted_at IS NULL
             AND (
               message.expires_at IS NULL
               OR message.expires_at > clock_timestamp()
             )",
    )
    .bind(result.to_uuid())
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound("search result not found".into()))?;
    Ok(RoomId::from_uuid(room))
}

async fn lock_live_result(
    tx: &mut Transaction<'_, Postgres>,
    result: MessageId,
    room: RoomId,
    workspace: WorkspaceId,
) -> Result<(), Error> {
    let locked = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT message.id
            FROM messages message
            JOIN rooms canonical_room ON canonical_room.id = message.room_id
           WHERE message.id = $1
             AND message.room_id = $2
             AND canonical_room.workspace_id = $3
             AND message.deleted_at IS NULL
             AND (
               message.expires_at IS NULL
               OR message.expires_at > clock_timestamp()
             )
           FOR SHARE OF message, canonical_room",
    )
    .bind(result.to_uuid())
    .bind(room.to_uuid())
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if locked.is_some() {
        Ok(())
    } else {
        Err(Error::NotFound("search result not found".into()))
    }
}

#[cfg(test)]
#[path = "search_feedback/db_tests.rs"]
mod db_tests;

#[cfg(test)]
#[path = "search_feedback/security_tests.rs"]
mod security_tests;
