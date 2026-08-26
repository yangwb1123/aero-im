//! Read-only health sampling for the governance audit outbox.
//!
//! This module is deliberately separate from the relay state machine.  It only
//! executes fixed `SELECT` statements and emits a snapshot to an injected
//! metrics registry; it never claims, settles, requeues, or mutates an outbox
//! row.  The server timer can therefore retain the last good gauge values when
//! a sample fails or the migration has not reached a rolling-upgrade instance.

use aero_common::metrics::{names, MetricKind, Registry};
use sqlx::PgPool;

/// SQL mirror of [`aero_eng::audit_provision::Q3_SQL`].  The root-crate
/// cross-pin test keeps the two textual oracles identical.
pub const Q3_SQL: &str = "SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status";

/// SQL mirror of [`aero_eng::audit_provision::Q4_SQL`].  The `{table}` slot is
/// replaced only with the fixed schema-owned table name below.
pub const Q4_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 0";

const TABLE_NAME: &str = "audit_governance_outbox";

/// One read-only snapshot of the four status buckets and oldest pending age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditOutboxCounts {
    /// Status 0: pending/enqueued rows.
    pub pending: i64,
    /// Status 1: claimed rows with a live lease.
    pub claimed: i64,
    /// Status 2: successfully delivered rows.
    pub delivered: i64,
    /// Status 3: terminal dead rows requiring manual recovery.
    pub dead: i64,
    /// Age of the oldest status-0 row in seconds; `None` means no pending row.
    pub oldest_pending_secs: Option<i64>,
}

/// Sample the governance outbox without touching its state machine.
///
/// A missing table is expected during a rolling upgrade and returns `Ok(None)`
/// so callers can preserve the previous gauge values.  Any other database
/// error is returned to the caller for warning/observability handling.
pub async fn sample_outbox_health(pool: &PgPool) -> Result<Option<AuditOutboxCounts>, sqlx::Error> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('audit_governance_outbox') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !present {
        return Ok(None);
    }

    let q3 = Q3_SQL.replace("{table}", TABLE_NAME);
    let rows: Vec<(i32, i64)> = sqlx::query_as(&q3).fetch_all(pool).await?;
    let mut buckets = [0_i64; 4];
    for (status, count) in rows {
        if let Ok(index) = usize::try_from(status) {
            if let Some(bucket) = buckets.get_mut(index) {
                *bucket = count;
            }
        }
    }

    let q4 = Q4_SQL.replace("{table}", TABLE_NAME);
    let oldest_pending_secs: Option<i64> = sqlx::query_scalar(&q4).fetch_one(pool).await?;

    Ok(Some(AuditOutboxCounts {
        pending: buckets[0],
        claimed: buckets[1],
        delivered: buckets[2],
        dead: buckets[3],
        oldest_pending_secs,
    }))
}

/// Emit one snapshot into an injected registry.
///
/// This remains a useful library/test emitter. The server's runtime owner is
/// the B5-4 Tier-1/Tier-2 status-label sampler; server boot must not call this
/// helper because it would also write the owned oldest-pending series.
pub fn set_audit_outbox_gauges(reg: &Registry, counts: &AuditOutboxCounts) {
    reg.register_help(
        names::AUDIT_OUTBOX_PENDING,
        MetricKind::Gauge,
        "Audit governance outbox rows in status 0 (pending/enqueued).",
    );
    reg.register_help(
        names::AUDIT_OUTBOX_CLAIMED,
        MetricKind::Gauge,
        "Audit governance outbox rows in status 1 (claimed, lease held).",
    );
    reg.register_help(
        names::AUDIT_OUTBOX_DELIVERED,
        MetricKind::Gauge,
        "Audit governance outbox rows in status 2 (delivered).",
    );
    reg.register_help(
        names::AUDIT_OUTBOX_DEAD,
        MetricKind::Gauge,
        "Audit governance outbox rows in status 3 (dead, terminal).",
    );
    reg.register_help(
        names::AUDIT_OUTBOX_OLDEST_PENDING_SECS,
        MetricKind::Gauge,
        "Age in seconds of the oldest status-0 pending audit outbox row.",
    );

    // Gauge counts are operationally far below 2^53; these conversions are
    // exact for all realistic database cardinalities.
    #[allow(clippy::cast_precision_loss)]
    {
        reg.set_gauge(names::AUDIT_OUTBOX_PENDING, counts.pending as f64);
        reg.set_gauge(names::AUDIT_OUTBOX_CLAIMED, counts.claimed as f64);
        reg.set_gauge(names::AUDIT_OUTBOX_DELIVERED, counts.delivered as f64);
        reg.set_gauge(names::AUDIT_OUTBOX_DEAD, counts.dead as f64);
        reg.set_gauge(
            names::AUDIT_OUTBOX_OLDEST_PENDING_SECS,
            counts.oldest_pending_secs.unwrap_or(0) as f64,
        );
    }
}

/// Return whether a dead-row warning should fire for this sample.
///
/// The first observation is not noisy; only two consecutive samples with a
/// non-zero terminal bucket indicate persistence beyond one poll interval.
#[must_use]
pub const fn dead_alarm(previous_dead: i64, current_dead: i64) -> bool {
    previous_dead > 0 && current_dead > 0
}

#[cfg(test)]
mod tests {
    use super::{dead_alarm, set_audit_outbox_gauges, AuditOutboxCounts};
    use aero_common::metrics::Registry;

    #[test]
    fn dead_alarm_only_fires_for_persistent_dead_rows() {
        assert!(!dead_alarm(0, 0));
        assert!(!dead_alarm(0, 3));
        assert!(!dead_alarm(3, 0));
        assert!(dead_alarm(3, 3));
    }

    #[test]
    fn gauge_emission_uses_the_five_pinned_series() {
        let registry = Registry::new();
        set_audit_outbox_gauges(
            &registry,
            &AuditOutboxCounts {
                pending: 4,
                claimed: 3,
                delivered: 2,
                dead: 1,
                oldest_pending_secs: Some(42),
            },
        );
        let rendered = registry.render_prometheus();
        for (name, value) in [
            ("aero_audit_outbox_pending", "4"),
            ("aero_audit_outbox_claimed", "3"),
            ("aero_audit_outbox_delivered", "2"),
            ("aero_audit_outbox_dead", "1"),
            ("aero_audit_outbox_oldest_pending_secs", "42"),
        ] {
            assert!(
                rendered.contains(&format!("{name} {value}")),
                "{name}: {rendered}"
            );
        }
        assert!(rendered.contains("oldest status-0 pending"));
    }

    #[test]
    fn no_pending_rows_render_oldest_age_as_zero() {
        let registry = Registry::new();
        set_audit_outbox_gauges(
            &registry,
            &AuditOutboxCounts {
                pending: 0,
                claimed: 0,
                delivered: 0,
                dead: 0,
                oldest_pending_secs: None,
            },
        );
        assert!(registry
            .render_prometheus()
            .contains("aero_audit_outbox_oldest_pending_secs 0"));
    }
}
