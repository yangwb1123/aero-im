//! Durable Snaplink entitlement projection and delivery outbox.
//!
//! `PostgreSQL` triggers create usage/audit deliveries in the same transaction as
//! their source rows.  This repository owns desired-state binding activation,
//! monotonic entitlement projection, and fenced at-least-once delivery leases.

use std::collections::{HashMap, HashSet};
use std::time::Duration as StdDuration;

use aero_common::WorkspaceId;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_CLAIM: i32 = 500;
const MAX_ERROR_CHARS: usize = 2_048;
const MAX_BACKOFF_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnaplinkBindingSpec {
    pub workspace_id: WorkspaceId,
    pub tenant_id: String,
    pub billing_client_id: String,
    pub audit_client_id: String,
    pub source_system: String,
    pub revision: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnaplinkLimitProjection {
    pub soft: i64,
    pub hard: i64,
    pub unlimited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnaplinkEntitlementProjection {
    pub workspace_id: WorkspaceId,
    pub tenant_id: String,
    pub revision: i64,
    pub active: bool,
    pub im_enabled: bool,
    pub notifications_enabled: bool,
    pub messages: SnaplinkLimitProjection,
    pub notifications: SnaplinkLimitProjection,
    pub effective_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionOutcome {
    Applied,
    Unchanged,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnaplinkDeliveryDestination {
    Usage,
    Audit,
}

impl SnaplinkDeliveryDestination {
    fn parse(value: &str) -> Result<Self, sqlx::Error> {
        match value {
            "usage" => Ok(Self::Usage),
            "audit" => Ok(Self::Audit),
            _ => Err(protocol_error("unknown Snaplink delivery destination")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnaplinkDeliveryClaim {
    pub delivery_id: String,
    pub destination: SnaplinkDeliveryDestination,
    pub workspace_id: WorkspaceId,
    pub tenant_id: String,
    pub client_id: String,
    pub source_system: String,
    pub idempotency_key: String,
    pub payload: serde_json::Value,
    pub occurred_at: OffsetDateTime,
    pub attempts: i64,
    pub claim_token: Uuid,
    pub lease_expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct StoredBinding {
    workspace_id: Uuid,
    tenant_id: String,
    billing_client_id: String,
    audit_client_id: String,
    source_system: String,
    revision: i64,
    enabled: bool,
}

#[derive(Debug, FromRow)]
struct StoredProjection {
    workspace_id: Uuid,
    tenant_id: String,
    revision: i64,
    active: bool,
    im_enabled: bool,
    notifications_enabled: bool,
    messages_soft: i64,
    messages_hard: i64,
    messages_unlimited: bool,
    notifications_soft: i64,
    notifications_hard: i64,
    notifications_unlimited: bool,
    effective_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
    generated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct DeliveryRow {
    delivery_id: String,
    destination: String,
    workspace_id: Uuid,
    tenant_id: String,
    client_id: String,
    source_system: String,
    idempotency_key: String,
    payload: serde_json::Value,
    occurred_at: OffsetDateTime,
    attempts: i64,
    claim_token: Uuid,
    lease_expires_at: OffsetDateTime,
}

impl TryFrom<DeliveryRow> for SnaplinkDeliveryClaim {
    type Error = sqlx::Error;

    fn try_from(row: DeliveryRow) -> Result<Self, Self::Error> {
        Ok(Self {
            delivery_id: row.delivery_id,
            destination: SnaplinkDeliveryDestination::parse(&row.destination)?,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            tenant_id: row.tenant_id,
            client_id: row.client_id,
            source_system: row.source_system,
            idempotency_key: row.idempotency_key,
            payload: row.payload,
            occurred_at: row.occurred_at,
            attempts: row.attempts,
            claim_token: row.claim_token,
            lease_expires_at: row.lease_expires_at,
        })
    }
}

#[derive(Clone)]
pub struct SnaplinkCommercialRepo {
    pool: PgPool,
}

impl SnaplinkCommercialRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Activate a complete desired-state binding set.
    ///
    /// Existing identities are immutable, revisions advance consecutively, and
    /// every existing workspace must have an enabled binding before the global
    /// database enforcement switch is committed.
    pub async fn configure_enabled(
        &self,
        desired: &[SnaplinkBindingSpec],
    ) -> Result<(), sqlx::Error> {
        validate_desired_bindings(desired)?;
        let mut tx = self.pool.begin().await?;
        let stored_by_workspace = lock_commercial_bindings(&mut tx).await?;
        reject_omitted_enabled_bindings(&stored_by_workspace, desired)?;
        for binding in desired {
            apply_binding(
                &mut tx,
                stored_by_workspace.get(&binding.workspace_id.to_uuid()),
                binding,
            )
            .await?;
        }
        require_complete_workspace_coverage(&mut tx).await?;
        sqlx::query(
            r"UPDATE snaplink_commercial_runtime
                  SET enabled = TRUE, updated_at = clock_timestamp()
                WHERE singleton",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    pub async fn configure_disabled(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE snaplink_commercial_runtime
                  SET enabled = FALSE, updated_at = clock_timestamp()
                WHERE singleton",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether `workspace` has an enabled commercial binding (D13 anchor for
    /// the F1 regression pair — the 0236 v1 trigger's binding lookup outcome
    /// for the auth pair's workspace).
    pub async fn has_enabled_binding(&self, workspace: WorkspaceId) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM snaplink_commercial_bindings
                             WHERE workspace_id = $1 AND enabled)",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await
    }

    pub async fn require_disabled(&self) -> Result<(), sqlx::Error> {
        let enabled = sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM snaplink_commercial_runtime WHERE singleton",
        )
        .fetch_one(&self.pool)
        .await?;
        if enabled {
            return Err(protocol_error(
                "database commercial enforcement is enabled but this process has no explicit configuration",
            ));
        }
        Ok(())
    }

    /// Install only a newer entitlement projection. Equal revisions must be
    /// byte-for-byte equivalent in all enforcement fields.
    pub async fn project_entitlement(
        &self,
        projection: &SnaplinkEntitlementProjection,
    ) -> Result<ProjectionOutcome, sqlx::Error> {
        validate_projection(projection)?;
        let mut tx = self.pool.begin().await?;
        let tenant = sqlx::query_scalar::<_, String>(
            r"SELECT tenant_id
                FROM snaplink_commercial_bindings
               WHERE workspace_id = $1 AND enabled
               FOR SHARE",
        )
        .bind(projection.workspace_id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| protocol_error("Snaplink binding is unavailable for projection"))?;
        if tenant != projection.tenant_id {
            return Err(protocol_error(
                "Snaplink entitlement tenant does not match its trusted binding",
            ));
        }
        let existing = fetch_projection(&mut tx, projection.workspace_id).await?;
        let outcome = match existing {
            Some(existing) if existing.revision > projection.revision => ProjectionOutcome::Stale,
            Some(existing) if existing.revision == projection.revision => {
                if !projection_matches(&existing, projection) {
                    return Err(protocol_error(
                        "same-revision Snaplink entitlement changed enforcement fields",
                    ));
                }
                ProjectionOutcome::Unchanged
            }
            Some(_) => {
                update_projection(&mut tx, projection).await?;
                ProjectionOutcome::Applied
            }
            None => {
                insert_projection(&mut tx, projection).await?;
                ProjectionOutcome::Applied
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Readiness is projection-based. Remote reachability and delivery backlog
    /// deliberately do not participate, so a central outage cannot evict a pod
    /// while its last durable entitlement remains effective.
    pub async fn ready(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            r"SELECT runtime.enabled
                  AND NOT EXISTS (
                        SELECT 1
                          FROM workspaces workspace
                          LEFT JOIN snaplink_commercial_bindings binding
                            ON binding.workspace_id = workspace.id AND binding.enabled
                          LEFT JOIN snaplink_entitlement_projections projection
                            ON projection.workspace_id = workspace.id
                           AND projection.tenant_id = binding.tenant_id
                         WHERE binding.workspace_id IS NULL
                            OR projection.workspace_id IS NULL
                            OR NOT projection.active
                            OR projection.effective_at > clock_timestamp()
                            OR (projection.expires_at IS NOT NULL
                                AND projection.expires_at <= clock_timestamp())
                  )
               FROM snaplink_commercial_runtime runtime
              WHERE runtime.singleton",
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: StdDuration,
        limit: i64,
    ) -> Result<Vec<SnaplinkDeliveryClaim>, sqlx::Error> {
        let lease_expires_at = now
            + time::Duration::seconds(
                i64::try_from(lease.as_secs().clamp(1, 86_400)).unwrap_or(86_400),
            );
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"WITH claimable AS (
                  SELECT candidate.delivery_id
                    FROM snaplink_delivery_outbox candidate
                   WHERE candidate.delivered_at IS NULL
                     AND candidate.available_at <= $1
                     AND (candidate.lease_expires_at IS NULL
                          OR candidate.lease_expires_at <= $1)
                   ORDER BY candidate.available_at, candidate.created_at,
                            candidate.delivery_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $2
              )
              UPDATE snaplink_delivery_outbox outbox
                 SET claim_token = gen_random_uuid(),
                     lease_expires_at = $3,
                     attempts = outbox.attempts + 1
                FROM claimable
               WHERE outbox.delivery_id = claimable.delivery_id
           RETURNING outbox.delivery_id, outbox.destination, outbox.workspace_id,
                     outbox.tenant_id, outbox.client_id, outbox.source_system,
                     outbox.idempotency_key, outbox.payload, outbox.occurred_at,
                     outbox.attempts, outbox.claim_token, outbox.lease_expires_at",
        )
        .bind(now)
        .bind(limit.clamp(1, i64::from(MAX_CLAIM)))
        .bind(lease_expires_at)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn mark_delivered(&self, claim: &SnaplinkDeliveryClaim) -> Result<bool, sqlx::Error> {
        let updated = sqlx::query(
            r"UPDATE snaplink_delivery_outbox
                  SET delivered_at = clock_timestamp(), claim_token = NULL,
                      lease_expires_at = NULL, last_error = NULL
                WHERE delivery_id = $1
                  AND claim_token = $2
                  AND delivered_at IS NULL
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(&claim.delivery_id)
        .bind(claim.claim_token)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    pub async fn mark_failed(
        &self,
        claim: &SnaplinkDeliveryClaim,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let available_at = now + commercial_delivery_backoff(&claim.delivery_id, claim.attempts);
        let error = error.chars().take(MAX_ERROR_CHARS).collect::<String>();
        let updated = sqlx::query(
            r"UPDATE snaplink_delivery_outbox
                  SET available_at = $3, claim_token = NULL,
                      lease_expires_at = NULL, last_error = $4
                WHERE delivery_id = $1
                  AND claim_token = $2
                  AND delivered_at IS NULL
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(&claim.delivery_id)
        .bind(claim.claim_token)
        .bind(available_at)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    pub async fn pending_count(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM snaplink_delivery_outbox WHERE delivered_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Recover current-month usage accepted before commercial activation.
    /// Stable delivery ids make repeated batches and concurrent replicas safe.
    pub async fn reconcile_usage(&self, limit: i32) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar("SELECT aero_reconcile_snaplink_usage($1)")
            .bind(limit.clamp(1, MAX_CLAIM))
            .fetch_one(&self.pool)
            .await
    }

    /// Recover historical local audit rows in bounded batches. New audit rows
    /// are already atomically enqueued by the database trigger.
    pub async fn reconcile_audit(&self, limit: i32) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar("SELECT aero_reconcile_snaplink_audit($1)")
            .bind(limit.clamp(1, MAX_CLAIM))
            .fetch_one(&self.pool)
            .await
    }
}

async fn lock_commercial_bindings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<HashMap<Uuid, StoredBinding>, sqlx::Error> {
    sqlx::query("SELECT singleton FROM snaplink_commercial_runtime WHERE singleton FOR UPDATE")
        .execute(&mut **tx)
        .await?;
    let stored = sqlx::query_as::<_, StoredBinding>(
        r"SELECT workspace_id, tenant_id, client_id AS billing_client_id,
                  audit_client_id, source_system, revision, enabled
            FROM snaplink_commercial_bindings
           FOR UPDATE",
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(stored
        .into_iter()
        .map(|binding| (binding.workspace_id, binding))
        .collect())
}

fn reject_omitted_enabled_bindings(
    stored: &HashMap<Uuid, StoredBinding>,
    desired: &[SnaplinkBindingSpec],
) -> Result<(), sqlx::Error> {
    let desired_workspaces = desired
        .iter()
        .map(|binding| binding.workspace_id.to_uuid())
        .collect::<HashSet<_>>();
    if stored
        .values()
        .any(|binding| binding.enabled && !desired_workspaces.contains(&binding.workspace_id))
    {
        return Err(protocol_error(
            "enabled persisted Snaplink binding is absent from desired state",
        ));
    }
    Ok(())
}

async fn require_complete_workspace_coverage(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    let uncovered = sqlx::query_scalar::<_, i64>(
        r"SELECT count(*)
            FROM workspaces workspace
            LEFT JOIN snaplink_commercial_bindings binding
              ON binding.workspace_id = workspace.id AND binding.enabled
           WHERE binding.workspace_id IS NULL
             AND workspace.id <> '00000000-0000-0000-0000-000000000000'",
    )
    .fetch_one(&mut **tx)
    .await?;
    if uncovered != 0 {
        return Err(protocol_error(format!(
            "{uncovered} existing workspace(s) have no enabled Snaplink binding"
        )));
    }
    Ok(())
}

async fn apply_binding(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    stored: Option<&StoredBinding>,
    desired: &SnaplinkBindingSpec,
) -> Result<(), sqlx::Error> {
    let workspace = desired.workspace_id.to_uuid();
    match stored {
        None if desired.revision != 1 => Err(protocol_error(
            "new Snaplink binding must start at revision 1",
        )),
        None => {
            sqlx::query(
                r"INSERT INTO snaplink_commercial_bindings
                        (workspace_id, tenant_id, client_id, audit_client_id,
                         source_system, revision, enabled)
                  VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(workspace)
            .bind(&desired.tenant_id)
            .bind(&desired.billing_client_id)
            .bind(&desired.audit_client_id)
            .bind(&desired.source_system)
            .bind(desired.revision)
            .bind(desired.enabled)
            .execute(&mut **tx)
            .await?;
            Ok(())
        }
        Some(current) if !binding_identity_matches(current, desired) => Err(protocol_error(
            "Snaplink binding identity cannot be rebound",
        )),
        Some(current) if current.revision == desired.revision => {
            if current.enabled != desired.enabled
                || current.billing_client_id != desired.billing_client_id
                || current.audit_client_id != desired.audit_client_id
            {
                return Err(protocol_error(
                    "same-revision Snaplink binding changed desired state",
                ));
            }
            Ok(())
        }
        Some(current) if desired.revision != current.revision + 1 => Err(protocol_error(
            "Snaplink binding revision must advance consecutively",
        )),
        Some(_) => update_binding(tx, desired).await,
    }
}

async fn update_binding(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    desired: &SnaplinkBindingSpec,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"UPDATE snaplink_commercial_bindings
              SET revision = $2, enabled = $3, client_id = $4,
                  audit_client_id = $5,
                  updated_at = clock_timestamp()
            WHERE workspace_id = $1",
    )
    .bind(desired.workspace_id.to_uuid())
    .bind(desired.revision)
    .bind(desired.enabled)
    .bind(&desired.billing_client_id)
    .bind(&desired.audit_client_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn binding_identity_matches(stored: &StoredBinding, desired: &SnaplinkBindingSpec) -> bool {
    stored.workspace_id == desired.workspace_id.to_uuid()
        && stored.tenant_id == desired.tenant_id
        && stored.source_system == desired.source_system
}

fn validate_desired_bindings(desired: &[SnaplinkBindingSpec]) -> Result<(), sqlx::Error> {
    let mut workspaces = HashSet::new();
    let mut tenants = HashSet::new();
    let mut clients = HashSet::new();
    let mut sources = HashSet::new();
    for binding in desired {
        if binding.revision <= 0
            || !valid_binding_identity(&binding.tenant_id, 256)
            || !valid_binding_identity(&binding.billing_client_id, 512)
            || !valid_binding_identity(&binding.audit_client_id, 512)
            || !valid_binding_identity(&binding.source_system, 128)
            || !workspaces.insert(binding.workspace_id)
            || !tenants.insert(&binding.tenant_id)
            || !clients.insert(&binding.billing_client_id)
            || !clients.insert(&binding.audit_client_id)
            || !sources.insert(&binding.source_system)
        {
            return Err(protocol_error(
                "Snaplink desired bindings contain an invalid revision or duplicate identity",
            ));
        }
    }
    Ok(())
}

fn valid_binding_identity(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value == value.trim()
        && value.len() <= max
        && !value.chars().any(char::is_control)
}

fn validate_projection(value: &SnaplinkEntitlementProjection) -> Result<(), sqlx::Error> {
    if value.revision <= 0
        || value.tenant_id.trim() != value.tenant_id
        || value.tenant_id.is_empty()
        || value
            .expires_at
            .is_some_and(|expires| expires <= value.effective_at)
        || !valid_limit(value.messages)
        || !valid_limit(value.notifications)
    {
        return Err(protocol_error("invalid Snaplink entitlement projection"));
    }
    Ok(())
}

fn valid_limit(limit: SnaplinkLimitProjection) -> bool {
    if limit.soft < 0 || limit.hard < 0 {
        return false;
    }
    if limit.unlimited {
        limit.soft == 0 && limit.hard == 0
    } else {
        limit.soft <= limit.hard
    }
}

async fn fetch_projection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<Option<StoredProjection>, sqlx::Error> {
    sqlx::query_as::<_, StoredProjection>(
        r"SELECT workspace_id, tenant_id, revision, active, im_enabled,
                  notifications_enabled, messages_soft, messages_hard,
                  messages_unlimited, notifications_soft, notifications_hard,
                  notifications_unlimited, effective_at, expires_at, generated_at
             FROM snaplink_entitlement_projections
            WHERE workspace_id = $1
            FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await
}

fn projection_matches(
    stored: &StoredProjection,
    projected: &SnaplinkEntitlementProjection,
) -> bool {
    stored.workspace_id == projected.workspace_id.to_uuid()
        && stored.tenant_id == projected.tenant_id
        && stored.revision == projected.revision
        && stored.active == projected.active
        && stored.im_enabled == projected.im_enabled
        && stored.notifications_enabled == projected.notifications_enabled
        && stored.messages_soft == projected.messages.soft
        && stored.messages_hard == projected.messages.hard
        && stored.messages_unlimited == projected.messages.unlimited
        && stored.notifications_soft == projected.notifications.soft
        && stored.notifications_hard == projected.notifications.hard
        && stored.notifications_unlimited == projected.notifications.unlimited
        && stored.effective_at == projected.effective_at
        && stored.expires_at == projected.expires_at
        && stored.generated_at == projected.generated_at
}

async fn insert_projection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    value: &SnaplinkEntitlementProjection,
) -> Result<(), sqlx::Error> {
    projection_query(
        r"INSERT INTO snaplink_entitlement_projections
                (workspace_id, tenant_id, revision, active, im_enabled,
                 notifications_enabled, messages_soft, messages_hard,
                 messages_unlimited, notifications_soft, notifications_hard,
                 notifications_unlimited, effective_at, expires_at, generated_at)
          VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        tx,
        value,
    )
    .await
}

async fn update_projection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    value: &SnaplinkEntitlementProjection,
) -> Result<(), sqlx::Error> {
    projection_query(
        r"UPDATE snaplink_entitlement_projections
              SET tenant_id = $2, revision = $3, active = $4, im_enabled = $5,
                  notifications_enabled = $6, messages_soft = $7,
                  messages_hard = $8, messages_unlimited = $9,
                  notifications_soft = $10, notifications_hard = $11,
                  notifications_unlimited = $12, effective_at = $13,
                  expires_at = $14, generated_at = $15,
                  projected_at = clock_timestamp()
            WHERE workspace_id = $1",
        tx,
        value,
    )
    .await
}

async fn projection_query(
    sql: &str,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    value: &SnaplinkEntitlementProjection,
) -> Result<(), sqlx::Error> {
    sqlx::query(sql)
        .bind(value.workspace_id.to_uuid())
        .bind(&value.tenant_id)
        .bind(value.revision)
        .bind(value.active)
        .bind(value.im_enabled)
        .bind(value.notifications_enabled)
        .bind(value.messages.soft)
        .bind(value.messages.hard)
        .bind(value.messages.unlimited)
        .bind(value.notifications.soft)
        .bind(value.notifications.hard)
        .bind(value.notifications.unlimited)
        .bind(value.effective_at)
        .bind(value.expires_at)
        .bind(value.generated_at)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[must_use]
pub fn commercial_delivery_backoff(delivery_id: &str, attempts: i64) -> time::Duration {
    let exponent = u32::try_from(attempts.saturating_sub(1).clamp(0, 8)).unwrap_or(8);
    let base = 1_u64
        .checked_shl(exponent)
        .unwrap_or(256)
        .min(MAX_BACKOFF_SECS);
    let digest = Sha256::digest(delivery_id.as_bytes());
    let jitter_seed = u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix"));
    let jitter = jitter_seed % (base / 4 + 1);
    time::Duration::seconds(i64::try_from((base + jitter).min(MAX_BACKOFF_SECS)).unwrap_or(300))
}

fn protocol_error(message: impl Into<String>) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_hard_zero_and_unlimited_zero_are_distinct_and_valid() {
        assert!(valid_limit(SnaplinkLimitProjection {
            soft: 0,
            hard: 0,
            unlimited: false,
        }));
        assert!(valid_limit(SnaplinkLimitProjection {
            soft: 0,
            hard: 0,
            unlimited: true,
        }));
        assert!(!valid_limit(SnaplinkLimitProjection {
            soft: 0,
            hard: 1,
            unlimited: true,
        }));
    }

    #[test]
    fn delivery_backoff_is_bounded_and_deterministic() {
        let first = commercial_delivery_backoff("usage:a", 1);
        assert_eq!(first, commercial_delivery_backoff("usage:a", 1));
        assert!(first >= time::Duration::seconds(1));
        assert!(commercial_delivery_backoff("usage:a", i64::MAX) <= time::Duration::seconds(300));
    }
}
