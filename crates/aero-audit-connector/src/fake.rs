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
use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use std::sync::Mutex;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use aero_auth::relay_scope::RelayScopeProvisioner;
use aero_common::AuditId;

use crate::outbox::{Claim, Error, OutboxRepo, StatusBuckets, VerdictProbe};
use crate::relay::{audit_backoff, clamped_lease, min_service_floor, truncate_error, MAX_CLAIM};

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

/// Small deterministic scope provisioner for connector tests and local drills.
/// Production boot uses the database-backed implementation from
/// `aero_storage::SnaplinkCommercialRepo` instead.
#[derive(Debug, Clone, Copy)]
pub struct StaticScopeProvisioner {
    provisioned: bool,
}

impl StaticScopeProvisioner {
    #[must_use]
    pub const fn new(provisioned: bool) -> Self {
        Self { provisioned }
    }
}

#[async_trait::async_trait]
impl RelayScopeProvisioner for StaticScopeProvisioner {
    async fn audit_event_write_provisioned(&self) -> bool {
        self.provisioned
    }
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
        // B5-3 D-CAP two-arm mirror of the PG claim CTE (pg.rs): arm A takes
        // the top `limit − K` rows of the total order; arm B takes the `K`
        // earliest-due rows of the lowest-priority lane NOT already selected
        // by arm A (the PG `NOT EXISTS` exclusion — an overlap would
        // double-claim a row). The claimed set is identical to the uncapped
        // top-`limit` set whenever the high lane is underfull.
        let limit = limit.clamp(1, MAX_CLAIM); // parity with PG (QA F4: limit 0 → 1 row)
        let floor = min_service_floor(limit);
        let head_take = usize::try_from(limit - floor).unwrap_or(usize::MAX);
        let floor_take = usize::try_from(floor).unwrap_or(usize::MAX);
        let mut selected: HashSet<AuditId> = HashSet::new();
        for (id, _, _, _) in due.iter().take(head_take) {
            selected.insert(*id);
        }
        if let Some(min_priority) = due.iter().map(|(_, _, _, priority)| *priority).min() {
            let arm_b_ids: Vec<AuditId> = due
                .iter()
                .filter(|(id, _, _, priority)| *priority == min_priority && !selected.contains(id))
                .take(floor_take)
                .map(|(id, ..)| *id)
                .collect();
            selected.extend(arm_b_ids);
        }
        let mut claims = Vec::new();
        for (id, _, _, _) in due.iter().filter(|(id, ..)| selected.contains(id)) {
            let row = state
                .rows
                .get_mut(id)
                .expect("due row ids come from the same map");
            row.status = FakeStatus::Claimed;
            row.attempts += 1;
            row.claim_token = Some(Uuid::new_v4());
            row.lease_expires_at = Some(lease_expires_at);
            claims.push(Claim {
                event_id: *id,
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

    async fn verdict_probe(&self) -> Result<VerdictProbe, Error> {
        let state = self.state.lock().expect("fake outbox poisoned");
        let mut probe = VerdictProbe::default();
        for row in state.rows.values() {
            match row.status {
                FakeStatus::Ready => probe.enqueued += 1,
                FakeStatus::Claimed => probe.claimed += 1,
                FakeStatus::Dead => probe.has_dead = true,
                FakeStatus::Delivered => {}
            }
        }
        Ok(probe)
    }

    async fn status_buckets(&self) -> Result<StatusBuckets, Error> {
        let state = self.state.lock().expect("fake outbox poisoned");
        let mut buckets = StatusBuckets::default();
        for row in state.rows.values() {
            match row.status {
                FakeStatus::Ready => buckets.enqueued += 1,
                FakeStatus::Claimed => buckets.claimed += 1,
                FakeStatus::Delivered => buckets.delivered += 1,
                FakeStatus::Dead => buckets.dead += 1,
            }
        }
        Ok(buckets)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    /// QA F2 — fake two-arm split: 20 admin (priority 100) + 8 backlog
    /// (priority 10), `claim_due(lease, 10)` → exactly 9 admin (arm A) +
    /// 1 backlog (arm B, the earliest-due low-lane row), set-disjoint, all
    /// `attempts == 1`, distinct tokens, and the remaining 18 rows
    /// untouched. Mirrors the PG split the relay-level tests ride on.
    #[tokio::test]
    async fn two_arm_claim_reserves_min_priority_floor() {
        let fake = FakeOutbox::new();
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let mut admin = Vec::new();
        for j in 0..20 {
            let id = AuditId::from_uuid(Uuid::new_v4());
            fake.insert_lane(
                id,
                json!({"n": j}),
                t0 - Duration::seconds(200) + Duration::seconds(100 + j),
                100,
                "admin".into(),
            )
            .await;
            admin.push(id);
        }
        let mut backlog = Vec::new();
        for i in 0..8 {
            let id = AuditId::from_uuid(Uuid::new_v4());
            fake.insert_lane(
                id,
                json!({"n": i}),
                t0 - Duration::seconds(200) + Duration::seconds(i),
                10,
                "message".into(),
            )
            .await;
            backlog.push(id);
        }

        let claimed = fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 10);
        let admin_claims: Vec<_> = claimed.iter().filter(|c| c.priority == 100).collect();
        let backlog_claims: Vec<_> = claimed.iter().filter(|c| c.priority == 10).collect();
        assert_eq!(admin_claims.len(), 9, "arm A = top 9 admin rows");
        assert_eq!(backlog_claims.len(), 1, "arm B = 1 low-lane slot");
        assert_eq!(
            backlog_claims[0].event_id, backlog[0],
            "arm B takes the earliest-due backlog row"
        );
        let ids: HashSet<AuditId> = claimed.iter().map(|c| c.event_id).collect();
        assert_eq!(ids.len(), 10, "claims are set-disjoint (no arm overlap)");
        assert!(claimed.iter().all(|c| c.attempts == 1));
        let tokens: HashSet<Uuid> = claimed.iter().map(|c| c.claim_token).collect();
        assert_eq!(tokens.len(), 10, "every claim rotates a distinct token");
        for id in admin.into_iter().chain(backlog) {
            if !ids.contains(&id) {
                let row = fake.row(id).await.expect("remaining row");
                assert_eq!(
                    row.status,
                    FakeStatus::Ready,
                    "unclaimed rows stay untouched"
                );
                assert_eq!(row.attempts, 0);
            }
        }
    }

    /// QA F2 — identity property: 3 admin + 20 backlog, `claim_due(lease,
    /// 10)` → the claimed set is byte-identical to the uncapped top-10 of
    /// the total order (3 admin + 7 earliest backlog). Arm B only fills
    /// rows the total order would not have reached.
    #[tokio::test]
    async fn two_arm_identity_when_high_lane_underfull() {
        let fake = FakeOutbox::new();
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let mut admin = Vec::new();
        for j in 0..3 {
            let id = AuditId::from_uuid(Uuid::new_v4());
            fake.insert_lane(
                id,
                json!({"n": j}),
                t0 - Duration::seconds(200) + Duration::seconds(100 + j),
                100,
                "admin".into(),
            )
            .await;
            admin.push(id);
        }
        let mut backlog = Vec::new();
        for i in 0..20 {
            let id = AuditId::from_uuid(Uuid::new_v4());
            fake.insert_lane(
                id,
                json!({"n": i}),
                t0 - Duration::seconds(200) + Duration::seconds(i),
                10,
                "message".into(),
            )
            .await;
            backlog.push(id);
        }

        let claimed = fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("claim");
        let mut expected: HashSet<AuditId> = admin.into_iter().collect();
        expected.extend(backlog.into_iter().take(7));
        let claimed_ids: HashSet<AuditId> = claimed.iter().map(|c| c.event_id).collect();
        assert_eq!(
            claimed_ids, expected,
            "high lane underfull → claimed set == uncapped top-10"
        );
    }

    /// QA F4 — clamp parity with PG: `claim_due(lease, 0)` clamps to batch
    /// 1 (K = 0 → empty arm B) and claims exactly one row.
    #[tokio::test]
    async fn claim_due_zero_limit_clamps_to_one() {
        let fake = FakeOutbox::new();
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let id = AuditId::from_uuid(Uuid::new_v4());
        fake.insert(id, json!({"n": 1}), t0).await;
        let claimed = fake
            .claim_due(Duration::seconds(30), 0)
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1, "limit 0 clamps to batch 1 (PG parity)");
        assert_eq!(claimed[0].event_id, id);
        assert_eq!(claimed[0].attempts, 1);
    }

    /// QA scenario 11 — requeued-low-lane recovery: a low-lane row parked
    /// 1s in the future (backoff) is not claimable while not due — arm B
    /// backfills from the MIN lane (admin, no throughput tax) — and once
    /// due it re-enters the claim under the still-pending admin flood via
    /// arm B's 1-slot floor, never starved.
    #[tokio::test]
    async fn requeued_low_lane_row_enters_arm_b_when_due() {
        let fake = FakeOutbox::new();
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        for j in 0..20 {
            let id = AuditId::from_uuid(Uuid::new_v4());
            fake.insert_lane(
                id,
                json!({"n": j}),
                t0 - Duration::seconds(200) + Duration::seconds(100 + j),
                100,
                "admin".into(),
            )
            .await;
        }
        let low = AuditId::from_uuid(Uuid::new_v4());
        fake.insert_lane(
            low,
            json!({"n": "low"}),
            t0 + Duration::seconds(1),
            10,
            "message".into(),
        )
        .await;

        // Round 1: the low row is not due yet — arm B backfills admin from
        // the MIN lane, so the batch is still full (9 arm A + 1 arm B).
        let round1 = fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("round 1");
        assert_eq!(round1.len(), 10);
        assert!(
            round1.iter().all(|c| c.priority == 100),
            "low lane not due → arm B backfills admin (no throughput tax)"
        );

        // Clock advance: the requeued low row is due; under the still-pending
        // admin flood it must be claimed via arm B (1 slot), not starved.
        fake.set_now(Some(t0 + Duration::seconds(1))).await;
        let round2 = fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("round 2");
        assert_eq!(round2.len(), 10);
        assert_eq!(
            round2.iter().filter(|c| c.priority == 100).count(),
            9,
            "arm A stays admin-first"
        );
        assert_eq!(round2.iter().filter(|c| c.priority == 10).count(), 1);
        assert_eq!(
            round2.iter().find(|c| c.priority == 10).map(|c| c.event_id),
            Some(low),
            "the requeued low-lane row re-enters the claim once due"
        );
    }

    /// R1.5 — sampling surfaces flip in step with the state machine: seed
    /// one row per status → `{1,1,true}` / `{1,1,1,1}`; `claim/settle/mark_dead`
    /// the enqueued row → both probes flip in step; an all-delivered subset →
    /// `{0,0,false}` / `{0,0,N,0}` (PG parity, no DB).
    #[tokio::test]
    async fn sampling_surfaces_flip_in_step_with_the_state_machine() {
        let fake = FakeOutbox::new();
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let ids: Vec<AuditId> = (0..4).map(|_| AuditId::from_uuid(Uuid::new_v4())).collect();
        for (idx, id) in ids.iter().enumerate() {
            fake.insert(*id, json!({"n": idx}), t0 - Duration::seconds(200))
                .await;
        }
        assert_eq!(
            fake.verdict_probe().await.expect("probe"),
            VerdictProbe {
                enqueued: 4,
                claimed: 0,
                has_dead: false
            },
            "all ready rows are enqueued"
        );
        assert_eq!(
            fake.status_buckets().await.expect("buckets"),
            StatusBuckets {
                enqueued: 4,
                claimed: 0,
                delivered: 0,
                dead: 0
            }
        );

        // Drive one row through claim → settle, one to dead, leaving two ready.
        let claimed = fake
            .claim_due(Duration::seconds(30), 4)
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 4);
        let token_for = |id: AuditId| {
            claimed
                .iter()
                .find(|claim| claim.event_id == id)
                .expect("claimed row")
                .claim_token
        };
        assert!(fake
            .settle(ids[0], token_for(ids[0]))
            .await
            .expect("settle"));
        assert!(fake
            .mark_dead(ids[1], token_for(ids[1]), 1, "drill dead")
            .await
            .expect("mark dead"));
        // ids[2]/ids[3] stay claimed (leases live; no requeue) — the probe
        // surface mirrors PG: claimed counts leased-but-unacked rows.
        assert_eq!(
            fake.verdict_probe().await.expect("probe"),
            VerdictProbe {
                enqueued: 0,
                claimed: 2,
                has_dead: true
            },
            "claim/settle/dead flip the Tier-1 probe in step"
        );
        assert_eq!(
            fake.status_buckets().await.expect("buckets"),
            StatusBuckets {
                enqueued: 0,
                claimed: 2,
                delivered: 1,
                dead: 1
            },
            "Tier-2 buckets flip in step"
        );
    }
}
