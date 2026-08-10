//! In-memory [`OutboxRepo`] test double.
//!
//! Reproduces the exact transition semantics of the PG implementation: claim
//! rotates a fresh token and advances `attempts`; every mutation is fenced on
//! `claim_token` (+ `attempts` for re-park/terminal) and an unexpired lease.
//! **Single clock domain**: the fake's own clock — `set_now`-pinned, real UTC
//! when unpinned — drives the claim filter, the lease mint, the requeue
//! `available_at` backoff arithmetic, and every fence, exactly as the PG
//! impl runs everything on `clock_timestamp()`. There is no caller-supplied
//! `now`, so an app clock skewed against the repo clock cannot enter the
//! state machine (the |skew| >= lease claim→POST→fence-fail livelock is
//! structurally impossible).

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde_json::Value;
use std::sync::Mutex;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use aero_common::AuditId;

use crate::outbox::{Claim, Error, OutboxRepo};
use crate::relay::{audit_backoff, clamped_lease, truncate_error};

/// Terminal/retry status of a fake row, mirroring the B5-1 0239 status enum
/// (0=enqueued, 1=claimed, 2=delivered, 3=dead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeStatus {
    Ready,
    Claimed,
    Delivered,
    Dead,
}

impl FakeStatus {
    /// Map onto the 0239 numeric enum for assertion parity with the PG impl.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ready => 0,
            Self::Claimed => 1,
            Self::Delivered => 2,
            Self::Dead => 3,
        }
    }
}

/// Public snapshot of one fake row for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRowSnapshot {
    pub status: FakeStatus,
    pub available_at: OffsetDateTime,
    pub attempts: i64,
    pub claim_token: Option<Uuid>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    /// B5-3 R3: 0239 lane column (SMALLINT; DESC claim term).
    pub priority: i16,
    /// B5-3 R3: 0239 lane column (TEXT; one of
    /// `aero_common::model::audit::GOVERNANCE_CLASS_*`).
    pub class: String,
}

#[derive(Debug, Clone)]
struct FakeRow {
    status: FakeStatus,
    available_at: OffsetDateTime,
    created_at: OffsetDateTime,
    attempts: i64,
    claim_token: Option<Uuid>,
    lease_expires_at: Option<OffsetDateTime>,
    last_error: Option<String>,
    payload: Value,
    priority: i16,
    class: String,
}

#[derive(Debug, Clone, Default)]
struct FakeState {
    rows: BTreeMap<AuditId, FakeRow>,
    /// Deterministic clock for the fenced transitions; `None` = real UTC now.
    clock: Option<OffsetDateTime>,
}

/// Public in-memory outbox implementing [`OutboxRepo`].
#[derive(Debug, Default)]
pub struct FakeOutbox {
    state: Mutex<FakeState>,
}

impl Clone for FakeOutbox {
    fn clone(&self) -> Self {
        let state = self.state.lock().expect("fake outbox poisoned");
        Self {
            state: Mutex::new(state.clone()),
        }
    }
}

impl FakeOutbox {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin the fake's clock (the single clock domain — the analog of PG's
    /// `clock_timestamp()`). `None` restores the real clock. Used by the
    /// claim filter, lease mint, backoff arithmetic, and every fence.
    pub async fn set_now(&self, now: Option<OffsetDateTime>) {
        self.state.lock().expect("fake outbox poisoned").clock = now;
    }

    fn clock(state: &FakeState) -> OffsetDateTime {
        state.clock.unwrap_or_else(OffsetDateTime::now_utc)
    }

    /// Seed a ready row due at `due_at` with zero attempts in the default
    /// backlog lane. Delegates to [`Self::insert_lane`] with the 0239 column
    /// defaults — `priority = 10` (`GOVERNANCE_PRIORITY_BACKLOG`,
    /// crates/aero-ai/src/governance.rs:33) and `class = "message"`
    /// (`aero_common::model::audit::GOVERNANCE_CLASS_MESSAGE`) — so every
    /// existing call site keeps its FIFO-in-backlog semantics.
    pub async fn insert(&self, event_id: AuditId, payload: Value, due_at: OffsetDateTime) {
        // 0239 defaults, pinned: DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG
        // (crates/aero-ai/src/governance.rs:33); DEFAULT 'message' =
        // GOVERNANCE_CLASS_MESSAGE (aero_common::model::audit).
        self.insert_lane(event_id, payload, due_at, 10, "message".into())
            .await;
    }

    /// Seed a ready row due at `due_at` with zero attempts in the given
    /// governance lane. `priority`/`class` mirror the 0239 columns; the claim
    /// sort runs on `priority` first (DESC, higher = claimed first).
    pub async fn insert_lane(
        &self,
        event_id: AuditId,
        payload: Value,
        due_at: OffsetDateTime,
        priority: i16,
        class: String,
    ) {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        state.rows.insert(
            event_id,
            FakeRow {
                status: FakeStatus::Ready,
                available_at: due_at,
                created_at: due_at,
                attempts: 0,
                claim_token: None,
                lease_expires_at: None,
                last_error: None,
                payload,
                priority,
                class,
            },
        );
    }

    /// Test helper: make a row immediately claimable again (drops any lease
    /// and backoff), without going through a full requeue cycle.
    pub async fn make_due_now(&self, event_id: AuditId) {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        let now = Self::clock(&state);
        if let Some(row) = state.rows.get_mut(&event_id) {
            row.available_at = now;
            row.claim_token = None;
            row.lease_expires_at = None;
        }
    }

    /// Snapshot one row for assertions.
    pub async fn row(&self, event_id: AuditId) -> Option<FakeRowSnapshot> {
        let state = self.state.lock().expect("fake outbox poisoned");
        state.rows.get(&event_id).map(|row| FakeRowSnapshot {
            status: row.status,
            available_at: row.available_at,
            attempts: row.attempts,
            claim_token: row.claim_token,
            lease_expires_at: row.lease_expires_at,
            last_error: row.last_error.clone(),
            priority: row.priority,
            class: row.class.clone(),
        })
    }
}

#[async_trait::async_trait]
impl OutboxRepo for FakeOutbox {
    /// Fake has no disabled-window concept: no rows to backfill, always 0.
    async fn reconcile(&self, _limit: i64) -> Result<i64, Error> {
        Ok(0)
    }

    async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error> {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        // Single clock domain: the lease is minted on the fake's own clock
        // (PG: `clock_timestamp() + lease`), never on a caller-supplied now.
        let now = Self::clock(&state);
        let lease_expires_at = now + clamped_lease(lease);
        let mut due = state
            .rows
            .iter()
            .filter(|(_, row)| {
                // Claimable = enqueued/claimed-but-expired (status 0/1), due,
                // with no unexpired lease — mirrors the PG `status IN (0, 1)`
                // claim CTE where the claim state lives in token+lease.
                !matches!(row.status, FakeStatus::Delivered | FakeStatus::Dead)
                    && row.available_at <= now
                    && row.lease_expires_at.map_or(true, |expires| expires <= now)
            })
            .map(|(id, row)| (*id, row.available_at, row.created_at, row.priority))
            .collect::<Vec<_>>();
        // Byte-for-byte mirror of the PG claim CTE order
        // (`ORDER BY candidate.priority DESC, candidate.available_at,
        // candidate.created_at, candidate.event_id` — pg.rs): priority DESC
        // leads, then FIFO within a lane.
        due.sort_by_key(|(id, available_at, created_at, priority)| {
            (Reverse(*priority), *available_at, *created_at, *id)
        });
        let mut claims = Vec::new();
        let take = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
        for (id, _, _, _) in due.into_iter().take(take) {
            let row = state
                .rows
                .get_mut(&id)
                .expect("due row ids come from the same map");
            row.status = FakeStatus::Claimed;
            row.attempts += 1;
            row.claim_token = Some(Uuid::new_v4());
            row.lease_expires_at = Some(lease_expires_at);
            claims.push(Claim {
                event_id: id,
                claim_token: row.claim_token.expect("claim token was just rotated"),
                lease_expires_at: row.lease_expires_at.expect("lease was just assigned"),
                attempts: row.attempts,
                payload: row.payload.clone(),
                priority: row.priority,
                class: row.class.clone(),
            });
        }
        Ok(claims)
    }

    async fn settle(&self, event_id: AuditId, claim_token: Uuid) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        let now = Self::clock(&state);
        let Some(row) = state.rows.get_mut(&event_id) else {
            return Ok(false);
        };
        if row.status != FakeStatus::Claimed
            || row.claim_token != Some(claim_token)
            || row.lease_expires_at.map_or(true, |expires| expires <= now)
        {
            return Ok(false);
        }
        row.status = FakeStatus::Delivered;
        row.claim_token = None;
        row.lease_expires_at = None;
        row.last_error = None;
        Ok(true)
    }

    async fn requeue(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        // Fence and backoff arithmetic share the fake's single clock (PG:
        // `clock_timestamp()` for both the fence and `available_at`).
        let now = Self::clock(&state);
        let Some(row) = state.rows.get_mut(&event_id) else {
            return Ok(false);
        };
        if row.status != FakeStatus::Claimed
            || row.claim_token != Some(claim_token)
            || row.attempts != attempts
            || row.lease_expires_at.map_or(true, |expires| expires <= now)
        {
            return Ok(false);
        }
        row.status = FakeStatus::Ready;
        row.available_at = now + audit_backoff(attempts);
        row.claim_token = None;
        row.lease_expires_at = None;
        row.last_error = Some(truncate_error(error));
        Ok(true)
    }

    async fn mark_dead(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("fake outbox poisoned");
        let now = Self::clock(&state);
        let Some(row) = state.rows.get_mut(&event_id) else {
            return Ok(false);
        };
        if row.status != FakeStatus::Claimed
            || row.claim_token != Some(claim_token)
            || row.attempts != attempts
            || row.lease_expires_at.map_or(true, |expires| expires <= now)
        {
            return Ok(false);
        }
        row.status = FakeStatus::Dead;
        row.claim_token = None;
        row.lease_expires_at = None;
        row.last_error = Some(truncate_error(error));
        Ok(true)
    }
}
