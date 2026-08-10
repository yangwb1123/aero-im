//! Durable paid-usage seam shared by queued and realtime AI calls.

use aero_common::metrics::Registry;
use aero_storage::AiJobKind;

use crate::metrics::{charge_cost, charge_cost_label, kind_label, CostModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub usage_id: uuid::Uuid,
    pub workspace: Option<uuid::Uuid>,
    pub kind: String,
    pub micros: u64,
    /// Versioned schema expected for a replayable provider result. Cost-only
    /// compatibility callers leave this unset.
    pub outcome_kind: Option<String>,
}

/// Minimal provider result persisted atomically with a finalized charge.
///
/// The versioned `kind` prevents a stable operation id from being decoded as a
/// different result shape after a deploy. `payload` contains only what the
/// caller needs to resume its business write; prompts are never copied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageOutcome {
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePersistOutcome {
    Inserted,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageReservation {
    pub usage_id: uuid::Uuid,
    pub token: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageReserveOutcome {
    Acquired(UsageReservation),
    InFlight,
    /// The stable provider operation completed earlier. `Some` replays its
    /// minimal result; `None` denotes conservative ambiguous settlement or a
    /// legacy cost-only operation and cannot safely synthesize a provider result.
    AlreadyFinalized(Option<UsageOutcome>),
}

/// Composition-root persistence seam. Paid provider paths reserve before the
/// network request, then finalize actual cost or cancel ordinary failure.
/// Zero-cost local heuristics never call this seam.
#[async_trait::async_trait]
pub trait UsageSink: Send + Sync {
    async fn reserve(&self, event: UsageEvent) -> Result<UsageReserveOutcome, String>;

    async fn finalize(
        &self,
        reservation: UsageReservation,
        actual_micros: u64,
        outcome: Option<UsageOutcome>,
    ) -> Result<UsagePersistOutcome, String>;

    async fn cancel(&self, reservation: UsageReservation) -> Result<bool, String>;

    /// Atomically-shaped compatibility helper for costs computed outside the
    /// provider wrappers. Production provider calls use the explicit three-step
    /// protocol so the reservation is durable before external spend.
    async fn persist(&self, event: UsageEvent) -> Result<UsagePersistOutcome, String> {
        let actual_micros = event.micros;
        let usage_id = event.usage_id;
        match self.reserve(event).await? {
            UsageReserveOutcome::Acquired(reservation) => {
                self.finalize(reservation, actual_micros, None).await
            }
            UsageReserveOutcome::AlreadyFinalized(_) => Ok(UsagePersistOutcome::Duplicate),
            UsageReserveOutcome::InFlight => Err(format!(
                "AI usage {usage_id} already has an active provider reservation"
            )),
        }
    }
}

const USAGE_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x9bf8_a524_f935_5f8a_b1f4_6cb8_9280_46bb);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageContext {
    pub root_id: uuid::Uuid,
    pub workspace: Option<uuid::Uuid>,
}

impl UsageContext {
    #[must_use]
    pub fn new(workspace: Option<uuid::Uuid>) -> Self {
        Self {
            root_id: uuid::Uuid::now_v7(),
            workspace,
        }
    }

    #[must_use]
    pub fn for_job(job_id: ulid::Ulid, workspace: Option<uuid::Uuid>) -> Self {
        Self {
            root_id: usage_id_for_job(job_id),
            workspace,
        }
    }

    #[must_use]
    pub fn for_message(message_id: uuid::Uuid, workspace: Option<uuid::Uuid>) -> Self {
        Self {
            root_id: usage_id_for_moderation(message_id),
            workspace,
        }
    }

    /// Deterministic root for an explicitly idempotent HTTP request. The actor
    /// and request fingerprint prevent a key reused in another tenant, endpoint,
    /// resource, or payload from aliasing the original provider operations.
    #[must_use]
    pub fn for_request(
        idempotency_key: &str,
        actor: uuid::Uuid,
        request_fingerprint: &str,
        workspace: Option<uuid::Uuid>,
    ) -> Self {
        let name = format!(
            "{actor}\0{}\0{request_fingerprint}\0{idempotency_key}",
            workspace.map_or_else(|| "none".to_owned(), |id| id.to_string())
        );
        Self {
            root_id: uuid::Uuid::new_v5(&USAGE_NAMESPACE, name.as_bytes()),
            workspace,
        }
    }

    #[must_use]
    pub fn operation_id(self, operation: &str) -> uuid::Uuid {
        uuid::Uuid::new_v5(&self.root_id, operation.as_bytes())
    }
}

#[must_use]
pub fn usage_id_for_job(job_id: ulid::Ulid) -> uuid::Uuid {
    let mut name = [0_u8; 17];
    name[0] = 1;
    name[1..].copy_from_slice(&job_id.0.to_be_bytes());
    uuid::Uuid::new_v5(&USAGE_NAMESPACE, &name)
}

#[must_use]
pub fn usage_id_for_moderation(message_id: uuid::Uuid) -> uuid::Uuid {
    let mut name = [0_u8; 17];
    name[0] = 2;
    name[1..].copy_from_slice(message_id.as_bytes());
    uuid::Uuid::new_v5(&USAGE_NAMESPACE, &name)
}

async fn persist_and_record(
    sink: Option<&dyn UsageSink>,
    usage_id: uuid::Uuid,
    reg: &Registry,
    kind: AiJobKind,
    workspace: Option<uuid::Uuid>,
    micros: u64,
) -> Result<UsagePersistOutcome, String> {
    if micros == 0 {
        charge_cost(reg, kind, workspace, 0);
        return Ok(UsagePersistOutcome::Duplicate);
    }
    let sink = sink.ok_or_else(|| {
        format!(
            "paid AI usage sink is not configured (usage_id={usage_id}, kind={})",
            kind_label(kind)
        )
    })?;
    let outcome = sink
        .persist(UsageEvent {
            usage_id,
            workspace,
            kind: kind_label(kind).to_owned(),
            micros,
            outcome_kind: None,
        })
        .await?;
    if outcome == UsagePersistOutcome::Inserted {
        charge_cost(reg, kind, workspace, micros);
    }
    Ok(outcome)
}

/// Persist a provider-specific cost whose kind is not an `ai_jobs` enum variant
/// (for example `translate`, `transcribe`, or a Voyage query embedding).
pub async fn record_durable_micros(
    sink: Option<&dyn UsageSink>,
    usage_id: uuid::Uuid,
    reg: &Registry,
    kind: &str,
    workspace: Option<uuid::Uuid>,
    micros: u64,
) -> Result<UsagePersistOutcome, String> {
    if micros == 0 {
        charge_cost_label(reg, kind, workspace, 0);
        return Ok(UsagePersistOutcome::Duplicate);
    }
    let sink = sink.ok_or_else(|| {
        format!("paid AI usage sink is not configured (usage_id={usage_id}, kind={kind})")
    })?;
    let outcome = sink
        .persist(UsageEvent {
            usage_id,
            workspace,
            kind: kind.to_owned(),
            micros,
            outcome_kind: None,
        })
        .await?;
    if outcome == UsagePersistOutcome::Inserted {
        charge_cost_label(reg, kind, workspace, micros);
    }
    Ok(outcome)
}

pub async fn record_durable_cost(
    sink: Option<&dyn UsageSink>,
    usage_id: uuid::Uuid,
    reg: &Registry,
    model: &CostModel,
    kind: AiJobKind,
    workspace: Option<uuid::Uuid>,
    paid: bool,
) -> Result<UsagePersistOutcome, String> {
    let micros = if paid { model.micros_for(kind) } else { 0 };
    persist_and_record(sink, usage_id, reg, kind, workspace, micros).await
}

// Internal accounting helper with a fixed parameter set; a context struct would
// churn both callers for no behavioral gain.
#[allow(clippy::too_many_arguments)]
pub async fn record_durable_token_cost(
    sink: Option<&dyn UsageSink>,
    usage_id: uuid::Uuid,
    reg: &Registry,
    model: &CostModel,
    kind: AiJobKind,
    workspace: Option<uuid::Uuid>,
    input_tokens: u32,
    output_tokens: u32,
) -> Result<UsagePersistOutcome, String> {    persist_and_record(
        sink,
        usage_id,
        reg,
        kind,
        workspace,
        model.token_micros(input_tokens, output_tokens),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<UsageEvent>>);

    #[async_trait::async_trait]
    impl UsageSink for Capture {
        async fn reserve(&self, event: UsageEvent) -> Result<UsageReserveOutcome, String> {
            self.0.lock().unwrap().push(event);
            Ok(UsageReserveOutcome::Acquired(UsageReservation {
                usage_id: self.0.lock().unwrap().last().unwrap().usage_id,
                token: uuid::Uuid::new_v4(),
            }))
        }

        async fn finalize(
            &self,
            _reservation: UsageReservation,
            _actual_micros: u64,
            _outcome: Option<UsageOutcome>,
        ) -> Result<UsagePersistOutcome, String> {
            Ok(UsagePersistOutcome::Inserted)
        }

        async fn cancel(&self, _reservation: UsageReservation) -> Result<bool, String> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn paid_is_persisted_and_unpaid_skips_sink() {
        let sink = Capture::default();
        let registry = Registry::new();
        let model = CostModel::default();
        let workspace = uuid::Uuid::new_v4();
        let usage_id = uuid::Uuid::new_v4();
        record_durable_cost(
            Some(&sink),
            usage_id,
            &registry,
            &model,
            AiJobKind::Answer,
            Some(workspace),
            true,
        )
        .await
        .unwrap();
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(events[0].usage_id, usage_id);
        assert_eq!(events[0].kind, "answer");

        record_durable_cost(
            Some(&sink),
            uuid::Uuid::new_v4(),
            &registry,
            &model,
            AiJobKind::Embed,
            Some(workspace),
            false,
        )
        .await
        .unwrap();
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn paid_without_sink_fails_closed() {
        let error = record_durable_cost(
            None,
            uuid::Uuid::new_v4(),
            &Registry::new(),
            &CostModel::default(),
            AiJobKind::Answer,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("not configured"));
    }

    #[test]
    fn ids_are_namespaced_and_repeatable() {
        let job = ulid::Ulid::new();
        let message = uuid::Uuid::new_v4();
        assert_eq!(usage_id_for_job(job), usage_id_for_job(job));
        assert_eq!(
            usage_id_for_moderation(message),
            usage_id_for_moderation(message)
        );
        assert_ne!(
            usage_id_for_job(ulid::Ulid(message.as_u128())),
            usage_id_for_moderation(message)
        );
    }

    #[test]
    fn request_ids_repeat_only_for_the_same_actor_scope_payload_and_key() {
        let actor = uuid::Uuid::new_v4();
        let workspace = Some(uuid::Uuid::new_v4());
        let first = UsageContext::for_request("retry-1", actor, "ask:room:q", workspace);
        assert_eq!(
            first,
            UsageContext::for_request("retry-1", actor, "ask:room:q", workspace)
        );
        assert_ne!(
            first,
            UsageContext::for_request("retry-2", actor, "ask:room:q", workspace)
        );
        assert_ne!(
            first,
            UsageContext::for_request("retry-1", actor, "ask:room:changed", workspace)
        );
        assert_ne!(
            first,
            UsageContext::for_request("retry-1", uuid::Uuid::new_v4(), "ask:room:q", workspace)
        );
    }
}
