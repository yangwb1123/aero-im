//! Minimal dependency-free counter registry for the audit relay (R-D2 §10.1).
//!
//! The runbook (`docs/runbooks/audit-relay-zero-delivery.md` §3) references
//! four metric names that previously had NO implementation — a sink outage or
//! dead-row backlog was silently invisible. This module makes the names real:
//! static atomics (no dependencies, no lock) + a Prometheus text renderer.
//! The server's observability gauge sampler (`bin/boot/metrics_tasks.rs`)
//! reads the counters every 30 s and re-exposes them via the shared
//! `aero_common::metrics` registry so `/metrics` serves them.
//!
//! Label vocabularies are fixed (bounded — never free-form labels):
//!   * `aero_audit_token_rejections_total{reason}` — reason ∈ {scope,
//!     claims, `unknown_key`, malformed, `unsupported_alg`, other};
//!   * `aero_audit_delivery_outcomes_total{outcome}` — outcome ∈
//!     {delivered, transient, permanent, forbidden, unprovisioned}.

use std::sync::atomic::{AtomicU64, Ordering};

/// Count of rows re-parked with backoff (transient failures and
/// permanent-class attempt-1 requeues).
pub const OUTBOX_TRANSIENT_REQUEUE: &str = "aero_audit_outbox_transient_requeue";
/// Count of rows transitioned to the dead terminal (status 3).
pub const OUTBOX_DEAD: &str = "aero_audit_outbox_dead";
/// Count of rejected tokens at the claims/signature plane.
pub const TOKEN_REJECTIONS_TOTAL: &str = "aero_audit_token_rejections_total";
/// Count of classified delivery outcomes (one per claimed row).
pub const DELIVERY_OUTCOMES_TOTAL: &str = "aero_audit_delivery_outcomes_total";

/// Token-rejection label vocabulary (runbook §3).
pub const TOKEN_REJECT_REASONS: [&str; 6] = [
    "scope",
    "claims",
    "unknown_key",
    "malformed",
    "unsupported_alg",
    "other",
];
/// Delivery-outcome label vocabulary (runbook §3).
pub const DELIVERY_OUTCOMES: [&str; 5] = [
    "delivered",
    "transient",
    "permanent",
    "forbidden",
    "unprovisioned",
];

static TRANSIENT_REQUEUE: AtomicU64 = AtomicU64::new(0);
static DEAD: AtomicU64 = AtomicU64::new(0);
static TOKEN_REJECTIONS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static DELIVERY_OUTCOMES_CNT: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// A relay transitioned a row to the requeue arm (transient OR permanent
/// attempt-1). Emit from `AuditRelay::deliver_claim`.
pub fn inc_transient_requeue() {
    TRANSIENT_REQUEUE.fetch_add(1, Ordering::Relaxed);
}

/// A relay transitioned a row to the dead terminal. Emit from
/// `AuditRelay::deliver_claim` (both the 403 immediate-death and the
/// `is_dead_at` permanent arms).
pub fn inc_dead() {
    DEAD.fetch_add(1, Ordering::Relaxed);
}

/// Classify a rejection into the bounded reason vocabulary and count it.
/// Emit from `AuditClient` at every claims/signature-plane rejection.
pub fn inc_token_rejection(reason: &str) {
    let idx = TOKEN_REJECT_REASONS
        .iter()
        .position(|candidate| *candidate == reason)
        .unwrap_or(5); // "other" — the vocabulary is closed; never grow labels
    TOKEN_REJECTIONS[idx].fetch_add(1, Ordering::Relaxed);
}

/// Map a client-side rejection reason string onto the runbook's bounded
/// `reason` vocabulary ({scope, claims, `unknown_key`, malformed,
/// `unsupported_alg`, other}). The fallback is `other` — never a new label.
#[must_use]
pub fn classify_token_rejection(reason: &str) -> &'static str {
    if reason.contains("scope") {
        "scope"
    } else if reason.contains("iss")
        || reason.contains("aud")
        || reason.contains("sub")
        || reason.contains("missing required fields")
        || reason.contains("expired")
        || reason.contains("not yet valid")
        || reason.contains("exp is not a number")
        || reason.contains("nbf is not a number")
    {
        "claims"
    } else if reason.contains("shape") || reason.contains("JWT") {
        "malformed"
    } else if reason.contains("alg") {
        "unsupported_alg"
    } else if reason.contains("kid") {
        "unknown_key"
    } else {
        "other"
    }
}

/// Count one classified delivery outcome. Emit from `AuditRelay::deliver_claim`
/// per claimed row.
pub fn inc_delivery_outcome(outcome: &str) {
    let idx = DELIVERY_OUTCOMES
        .iter()
        .position(|candidate| *candidate == outcome)
        .unwrap_or(4); // "unprovisioned" fallback — closed vocabulary
    DELIVERY_OUTCOMES_CNT[idx].fetch_add(1, Ordering::Relaxed);
}

/// Current counter values as (metric-name, label, value) triples — the
/// sampler leg's read surface.
/// One sampled counter: (metric-name, labels, value).
pub type SampledCounter = (&'static str, Vec<(&'static str, &'static str)>, u64);

#[must_use]
pub fn sample() -> Vec<SampledCounter> {
    let mut out = Vec::new();
    out.push((
        OUTBOX_TRANSIENT_REQUEUE,
        Vec::new(),
        TRANSIENT_REQUEUE.load(Ordering::Relaxed),
    ));
    out.push((OUTBOX_DEAD, Vec::new(), DEAD.load(Ordering::Relaxed)));
    for (idx, reason) in TOKEN_REJECT_REASONS.iter().enumerate() {
        out.push((
            TOKEN_REJECTIONS_TOTAL,
            vec![("reason", *reason)],
            TOKEN_REJECTIONS[idx].load(Ordering::Relaxed),
        ));
    }
    for (idx, outcome) in DELIVERY_OUTCOMES.iter().enumerate() {
        out.push((
            DELIVERY_OUTCOMES_TOTAL,
            vec![("outcome", *outcome)],
            DELIVERY_OUTCOMES_CNT[idx].load(Ordering::Relaxed),
        ));
    }
    out
}

/// Prometheus text render (the unit test's read-back surface; a future
/// standalone exporter could serve it directly). All-zero counters are
/// emitted — a present-but-zero line is observability; an absent line is
/// indistinguishable from a dropped emit.
#[must_use]
pub fn render() -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for (name, labels, value) in sample() {
        text.push_str(name);
        if !labels.is_empty() {
            let joined = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join(",");
            let _ = write!(text, "{{{joined}}}");
        }
        text.push(' ');
        text.push_str(&value.to_string());
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §10.6 AC — `delete_lane_metrics_registry_emits_and_reads_back`: emit
    /// each counter and read the Prometheus text render back; the runbook's
    /// four metric names and label vocabularies exist. Counters are global
    /// statics, so the test reads DELTAS around its own emits.
    #[test]
    fn delete_lane_metrics_registry_emits_and_reads_back() {
        let before_requeue = TRANSIENT_REQUEUE.load(Ordering::Relaxed);
        let before_dead = DEAD.load(Ordering::Relaxed);
        let before_rej: Vec<u64> = TOKEN_REJECTIONS
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let before_out: Vec<u64> = DELIVERY_OUTCOMES_CNT
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();

        inc_transient_requeue();
        inc_dead();
        for reason in TOKEN_REJECT_REASONS {
            inc_token_rejection(reason);
        }
        for outcome in DELIVERY_OUTCOMES {
            inc_delivery_outcome(outcome);
        }

        let text = render();
        for name in [
            OUTBOX_TRANSIENT_REQUEUE,
            OUTBOX_DEAD,
            TOKEN_REJECTIONS_TOTAL,
            DELIVERY_OUTCOMES_TOTAL,
        ] {
            assert!(
                text.lines().any(|line| line.starts_with(name)),
                "runbook metric {name} exists in the render"
            );
        }
        // The four metric names from the runbook §3 table.
        assert!(text.contains("aero_audit_outbox_transient_requeue"));
        assert!(text.contains("aero_audit_outbox_dead"));
        assert!(text.contains("aero_audit_token_rejections_total"));
        assert!(text.contains("aero_audit_delivery_outcomes_total"));
        // Label vocabulary lines present.
        for reason in TOKEN_REJECT_REASONS {
            assert!(
                text.contains(&format!("reason=\"{reason}\"")),
                "reason label {reason}"
            );
        }
        for outcome in DELIVERY_OUTCOMES {
            assert!(
                text.contains(&format!("outcome=\"{outcome}\"")),
                "outcome label {outcome}"
            );
        }
        // Deltas: each emit landed at least once. The counters are GLOBAL
        // statics shared with the relay/client tests (which emit the same
        // counters in parallel threads within this test binary), so the pin
        // is monotonic (`>=`), never an exact delta.
        assert!(
            TRANSIENT_REQUEUE.load(Ordering::Relaxed) > before_requeue,
            "transient-requeue incremented"
        );
        assert!(
            DEAD.load(Ordering::Relaxed) > before_dead,
            "dead incremented"
        );
        for (idx, reason) in TOKEN_REJECT_REASONS.iter().enumerate() {
            assert!(
                TOKEN_REJECTIONS[idx].load(Ordering::Relaxed) > before_rej[idx],
                "reason {reason} incremented"
            );
        }
        for (idx, outcome) in DELIVERY_OUTCOMES.iter().enumerate() {
            assert!(
                DELIVERY_OUTCOMES_CNT[idx].load(Ordering::Relaxed) > before_out[idx],
                "outcome {outcome} incremented"
            );
        }
    }

    /// Claim-plane validation failures and malformed JWT decoding failures
    /// map to their documented bounded metric labels.
    #[test]
    fn claim_rejection_classification_covers_claim_and_malformed_shapes() {
        for (reason, expected) in [
            ("token has expired", "claims"),
            ("token is not yet valid", "claims"),
            ("token exp is not a number", "claims"),
            ("token nbf is not a number", "claims"),
            ("token has no JWT header segment", "malformed"),
            ("JWT payload is not valid JSON", "malformed"),
        ] {
            assert_eq!(
                classify_token_rejection(reason),
                expected,
                "reason: {reason}"
            );
        }
    }

    /// Closed-vocabulary fallback: an unknown label maps to the terminal
    /// bucket and never grows the label set (bounded-cardinality invariant).
    #[test]
    fn unknown_labels_fall_back_to_terminal_buckets() {
        let before_other = TOKEN_REJECTIONS[5].load(Ordering::Relaxed);
        let before_unprovisioned = DELIVERY_OUTCOMES_CNT[4].load(Ordering::Relaxed);
        inc_token_rejection("never-seen-reason");
        inc_delivery_outcome("never-seen-outcome");
        assert!(TOKEN_REJECTIONS[5].load(Ordering::Relaxed) > before_other);
        assert!(DELIVERY_OUTCOMES_CNT[4].load(Ordering::Relaxed) > before_unprovisioned);
    }
}
