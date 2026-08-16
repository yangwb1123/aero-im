//! Outbox repository seam for the audit relay.
//!
//! Signatures mirror `AiUsageRepo::{claim_due, settle, mark_failed}`
//! (`crates/aero-storage/src/ai_usage.rs`) plus the new dead transition —
//! with one deliberate deviation: **the repo owns the clock exclusively**.
//! `claim_due`/`requeue` take no caller-supplied `now`; every lease mint,
//! availability filter, and fence is evaluated on a single clock domain
//! (PG: `clock_timestamp()`; the fake: its own injectable clock). The v1/
//! `ai_usage` pattern mints `lease_expires_at = app_now + lease` while fences
//! read the DB clock; when the app clock trails the DB clock by >= lease,
//! every fence fails and the claim filter never re-exposes the row — a
//! claim→POST→fence-fail livelock (no settle, no dead). Computing the lease
//! on the repo clock eliminates the |skew| >= lease liveness hazard.
//!
//! Every mutating operation is fenced on the rotated claim token: a stale
//! worker — superseded claim, mismatched attempt count, or expired lease —
//! receives `false`, never an error, so a late re-park from an old attempt
//! cannot overwrite a newer claim and a reclaimed row's old token can never
//! acknowledge (the `stale_claim_cannot_ack_after_reclaim` invariant).

use async_trait::async_trait;
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use aero_common::AuditId;

/// Tier-1 (30s) read-only outbox verdict probe (B5-4 R1). `enqueued`/
/// `claimed` are exact status-0/status-1 counts (QP1, partial-index-served);
/// `has_dead` is the EXISTS(status = 3) signal (QP2, O(1) via the
/// `audit_governance_status3_idx` partial index). Pure read — never a
/// fabricated zero snapshot: a missing table fails the whole probe (`Err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerdictProbe {
    pub enqueued: i64,
    pub claimed: i64,
    pub has_dead: bool,
}

/// Tier-2 (slow) full status-bucket aggregation (B5-4 R1), mirroring
/// `aero-eng` `Q3_SQL` (`GROUP BY status ORDER BY status`) exactly for CLI
/// oracle parity. Pure read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusBuckets {
    pub enqueued: i64,
    pub claimed: i64,
    pub delivered: i64,
    pub dead: i64,
}

/// One leased outbox row.
///
/// `event_id` is `AuditId` — the compile-time 1:1 with `audit_events.id`
/// (B5-1 parity; `audit_events.id` is a UUID column whose values are the
/// ULID-shaped `AuditId` UUID form — see `ids.rs define_id!`) and doubles as
/// the outbound `Idempotency-Key`, so a redelivery after a lost
/// acknowledgement is idempotent at the sink. The string form of `AuditId`
/// (ULID base32 via `Display`) is what the `Idempotency-Key` header carries;
/// the payload's embedded `event_id` field stays `uuid::text` (hyphenated
/// UUID) — `validate_audit_receipt` compares the receipt echo value-level
/// against both forms. `claim_token` is rotated per claim via
/// `gen_random_uuid()`. `attempts` is the post-increment delivery attempt
/// number (1 = first delivery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub event_id: AuditId,
    pub claim_token: Uuid,
    pub lease_expires_at: OffsetDateTime,
    pub attempts: i64,
    pub payload: Value,
    /// DESC lane value (higher = claimed first); 0239 SMALLINT.
    /// Pinned to `aero_ai::governance::GOVERNANCE_PRIORITY_*`
    /// (crates/aero-ai/src/governance.rs:31/:33).
    pub priority: i16,
    /// 0239 TEXT; one of `aero_common::model::audit::GOVERNANCE_CLASS_*`
    /// ("admin"/"message"/"room").
    pub class: String,
}

/// Durable outbox backing the audit relay.
#[async_trait]
pub trait OutboxRepo: Send + Sync {
    /// Disabled-window recovery (mirror of v1's `reconcile_audit`, which the
    /// v1 dispatch loop runs ahead of every claim batch): backfill up to
    /// `limit` v2 outbox rows for `message.moderated` audit rows accepted
    /// while the commercial runtime switch was off (the 0239 trigger never
    /// re-fires). Backfilled rows are byte-identical to trigger-enqueued
    /// rows and enter the same claim queue. Bounded + idempotent
    /// (`NOT EXISTS` + `ON CONFLICT (event_id) DO NOTHING`): re-runs return
    /// 0, concurrent runners never double-insert, dead rows are never
    /// resurrected. Returns the number of newly inserted rows.
    async fn reconcile(&self, limit: i64) -> Result<i64, Error>;

    /// Claim up to `limit` due rows: `available_at` reached and an expired or
    /// absent lease, ordered `(priority DESC, available_at, created_at,
    /// event_id)` — `priority DESC` first (B5-3: higher = more urgent, so
    /// the moderation lane claims before backlog regardless of enqueue
    /// order), then the FIFO tie-break — with `FOR UPDATE SKIP LOCKED`
    /// semantics, a fresh rotated token, and a lease minted on the
    /// repository's own clock (single clock domain — never a
    /// caller-supplied `now`, which could skew against the fences).
    /// The returned `Claim` carries the row's `priority`/`class` lane.
    ///
    /// B5-3 D-CAP (anti-starvation cap): the claimed set is two arms in one
    /// statement. Arm A takes the top `limit − K` rows of the total order;
    /// arm B takes the `K` earliest-due rows of the lowest-priority lane
    /// (today: priority 10 backlog) not already selected by arm A, where
    /// `K = min_service_floor(limit)`. Each claim round therefore reserves
    /// `K` slots for the lowest-priority lane; the claimed set equals the
    /// uncapped top-`limit` set whenever the high lane is underfull.
    async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error>;

    /// Fenced acknowledgement. Returns `false` (never an error) when the
    /// token no longer matches, the lease has expired, or the row is no
    /// longer claimable.
    async fn settle(&self, event_id: AuditId, claim_token: Uuid) -> Result<bool, Error>;

    /// Fenced re-park with exponential backoff capped at 300s. `available_at`
    /// is computed on the repository clock (`clock_timestamp() + backoff`),
    /// so the backoff arithmetic shares the claim filter's clock domain.
    /// Fenced on `claim_token AND attempts` so a late re-park from a
    /// superseded attempt cannot overwrite a newer claim.
    async fn requeue(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error>;

    /// Fenced terminal transition (dead-letter). The row is excluded from
    /// future claims forever; `error` is recorded as `last_error`.
    async fn mark_dead(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error>;

    /// Tier-1 (30s) read-only verdict probe: exact enqueued (status 0) /
    /// claimed (status 1) counts plus whether any dead (status 3) row exists.
    /// Never fabricates a zero snapshot — a missing table is `Err`.
    async fn verdict_probe(&self) -> Result<VerdictProbe, Error>;

    /// Tier-2 (slow) read-only full aggregation over the four status buckets
    /// (exact `Q3_SQL` mirror). Never fabricates a zero snapshot.
    async fn status_buckets(&self) -> Result<StatusBuckets, Error>;
}

/// Outbox store failure. Fence violations are *not* errors — they return
/// `false` from the mutating methods above.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audit outbox store failed: {0}")]
    Store(#[from] sqlx::Error),
}
