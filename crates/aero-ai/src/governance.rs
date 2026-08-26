//! Governance-lane mapping for local audit action tokens (B5-1 R1).
//!
//! Pure + unit-pinned, mirroring `aero_storage::ai_job::priority_for`
//! (ai_job.rs:62-68) as the lane *model* — NOT its numeric direction.
//!
//! ## Lane direction (do not "align" with `ai_job`)
//!
//! `ai_job::priority_for` is **ASC lower-first** ("lower runs first"). The
//! governance outbox claim (B5-3 `claim_due`) is **`priority DESC`
//! higher-first** — the INVERSE model. The R4 spec phrasing ("moderation lane
//! < message/room default lane") is a carry-over from the ASC model; the
//! pinning property here is *precedence* (moderation claimed first), which
//! under DESC means `GOVERNANCE_PRIORITY_MODERATION > GOVERNANCE_PRIORITY_BACKLOG`.
//! Do not "fix" the direction to match `ai_job` — that would invert the lane.
//!
//! ## Fail-closed / pass-through contract
//!
//! The mapping is keyed on the local audit **token only** — never caller
//! identity — so every `message.moderated` producer stamps identically. Unknown
//! tokens map to `None` (pass through unmapped): the shared 0239 trigger must
//! keep flowing non-moderation audit rows, so this function never raises,
//! never blocks, and never panics (a second abort path on unmapped tokens is
//! forbidden — the 0236 binding RAISE already aborts fail-closed on a missing
//! binding). `None` is also a deliberate negative: a moderation finalize
//! mis-tokened as `message.deleted` must NOT silently enter the admin lane
//! (R-D2) — see `user_delete_token_stays_out_of_admin_lane`.

use aero_storage::AiJob;

/// Governance claim ordering is `priority DESC` (B5-3: highest = first) — the
/// INVERSE of `ai_job`'s ASC lower-first model. Do not "align" these. Moderation
/// is the top lane.
pub const GOVERNANCE_PRIORITY_MODERATION: i16 = 100;
/// Default lane for message/room backlog rows (also the 0239 column default).
pub const GOVERNANCE_PRIORITY_BACKLOG: i16 = 10;

// ---- Vocabulary single-sourced at the leaf (`aero_common::model::audit`) ----
// The class spellings, the local moderation token, and the single outbound
// contract token are defined once at the leaf; this re-export chain keeps the
// `aero_ai::governance::*` and `aero_ai::*` paths intact for every existing
// consumer. A flip of `MODERATION_OUTBOUND_ACTION` is a one-line leaf edit
// and the A2/DB cross-pins follow automatically.
pub use aero_common::model::audit::{
    AGGREGATED_MESSAGE_ACTION, AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_ADMIN,
    GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, L1_WINDOW_SECONDS,
    LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MESSAGE_EDIT,
    LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MODERATED, LOCAL_ACTION_ROOM_ARCHIVED,
    LOCAL_ACTION_ROOM_CREATE, MODERATION_OUTBOUND_ACTION,
};

/// Authoritative vocabulary of local audit tokens that enter the admin lane.
/// Adding a new `admin.*` token requires both a vocabulary entry and a mapping
/// arm in [`governance_lane_for`]; the plain map-or-reject test prevents a
/// forgotten arm from silently passing through as an ordinary audit row.
pub const ADMIN_LANE_TOKENS: &[&str] = &[LOCAL_ACTION_MODERATED];

/// Governance tuple stamped onto a `snaplink_delivery_outbox` (v2) row by the
/// 0239 enqueue redirect, and the drill fixture's expected start state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernanceLane {
    /// One of [`GOVERNANCE_CLASS_ADMIN`] / [`GOVERNANCE_CLASS_MESSAGE`] /
    /// [`GOVERNANCE_CLASS_ROOM`].
    pub class: &'static str,
    /// DESC lane value (higher = claimed first, B5-3).
    pub priority: i16,
    /// Outbound contract token (e.g. [`MODERATION_OUTBOUND_ACTION`]).
    pub outbound_action: &'static str,
    /// Status 0 is the enqueue-time normative state (0239); a property of the
    /// outbox row lifecycle, pinned here as the drill fixture's start state.
    pub status: i16,
}

/// The complete moderation-finalize contract shared by the audit event and
/// the governance outbox row. This is the pure mirror of the tuple committed
/// by `AiWorker::handle_moderate` and asserted by the storage parity drill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModerationFinalizeContract {
    /// Local audit event action (`audit_events.action`).
    pub audit_action: &'static str,
    /// Governance outbox class.
    pub class: &'static str,
    /// Governance outbox priority (higher is claimed first).
    pub priority: i16,
    /// External governance action in the outbox payload.
    pub outbound_action: &'static str,
    /// Enqueue-time outbox status.
    pub status: i16,
}

/// Map a local audit action token → governance tuple.
///
/// Keyed on the token ONLY (never caller identity): every `message.moderated`
/// producer — `AiWorker::handle_moderate` (worker/mod.rs:373),
/// `ImService::moderate_delete` (`messages.rs`:632, incl. `moderation_bot`), and
/// `message_reports::review_authorized` (`message_reports.rs`:305, which passes
/// `actor=Some(reviewer)`) — maps identically.
///
/// Mapped tokens: `message.moderated` → admin lane (1:1, priority 100,
/// outbound `admin.content.flag`); `room.create`/`room.archived` → room lane
/// (1:1, priority 10, outbound = local token verbatim — the 0245 trigger's
/// SQL-side allowlist keys off these two, see `LOCAL_ACTION_ROOM_CREATE`).
///
/// Unknown tokens → `None` (pass through unmapped). Never raises, never
/// blocks: non-moderation audit rows must keep flowing through the shared
/// 0239 trigger/redirect.
#[must_use]
pub fn governance_lane_for(local_action: &str) -> Option<GovernanceLane> {
    match local_action {
        LOCAL_ACTION_MODERATED => Some(GovernanceLane {
            class: GOVERNANCE_CLASS_ADMIN,
            priority: GOVERNANCE_PRIORITY_MODERATION,
            outbound_action: MODERATION_OUTBOUND_ACTION,
            status: 0,
        }),
        // Room lane (migration 0245 `aero_enqueue_room_audit`): the local
        // token flows verbatim — no room contract token exists in
        // docs/proposals/audit-contract-batch-aero-im.md; a future mapping
        // is a one-line leaf edit. Outbound rows are produced ONLY by the
        // SQL trigger (Rust never writes the outbox for these tokens —
        // trigger-only ownership, 0245 file header). Each arm returns its
        // own static leaf const (the outbound field is &'static str; the
        // const IS the token, so verbatim == the const).
        LOCAL_ACTION_ROOM_CREATE => Some(GovernanceLane {
            class: GOVERNANCE_CLASS_ROOM,
            priority: GOVERNANCE_PRIORITY_BACKLOG, // 10 — same backlog lane as message windows
            outbound_action: LOCAL_ACTION_ROOM_CREATE, // verbatim — no fabricated contract token
            status: 0,
        }),
        LOCAL_ACTION_ROOM_ARCHIVED => Some(GovernanceLane {
            class: GOVERNANCE_CLASS_ROOM,
            priority: GOVERNANCE_PRIORITY_BACKLOG,
            outbound_action: LOCAL_ACTION_ROOM_ARCHIVED,
            status: 0,
        }),
        _ => None,
    }
}

/// Return the exact five-field contract implied by the moderation worker's
/// finalize path.
///
/// A contract exists only for a blocking verdict with a target message. The
/// workspace fail-closed decision remains in the caller's
/// `moderation_delete_workspace` guard; this function is intentionally pure,
/// total, and independent of tenant resolution or database state.
#[must_use]
pub fn finalize_contract_for(
    job: &AiJob,
    verdict: Option<&str>,
) -> Option<ModerationFinalizeContract> {
    if verdict.is_none() || job.target_id.is_none() {
        return None;
    }
    let lane = governance_lane_for(LOCAL_ACTION_MODERATED)?;
    Some(ModerationFinalizeContract {
        audit_action: LOCAL_ACTION_MODERATED,
        class: lane.class,
        priority: lane.priority,
        outbound_action: lane.outbound_action,
        status: lane.status,
    })
}

/// R5 classification for the L1 aggregation bypass (landed: migration 0242
/// `aero_enqueue_l1_aggregate_audit` + trigger `audit_events_l1_aggregate` —
/// the SQL-side allowlist is `LOCAL_ACTION_MESSAGE_CREATE`/`LOCAL_ACTION_MESSAGE_EDIT`):
/// admin-class rows must stay 1:1 (`event_id` = `audit_events.id`), never merged
/// by the high-volume `message.*` window.
#[must_use]
pub fn is_admin_class(local_action: &str) -> bool {
    matches!(
        governance_lane_for(local_action),
        Some(lane) if lane.class == GOVERNANCE_CLASS_ADMIN
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_storage::{AiJobKind, AiJobStatus};
    use time::OffsetDateTime;
    use ulid::Ulid;
    use uuid::Uuid;

    fn moderation_job(target_id: Option<Uuid>) -> AiJob {
        AiJob {
            id: Ulid::new(),
            kind: AiJobKind::Moderate,
            target_id,
            workspace_id: None,
            status: AiJobStatus::Queued,
            attempts: 0,
            payload: serde_json::Value::Null,
            result: None,
            error: None,
            scheduled_at: OffsetDateTime::now_utc(),
            started_at: None,
            finished_at: None,
        }
    }

    /// R4 lane precedence under the DESC claim model. `ai_job::priority_for`
    /// is ASC lower-first; the governance outbox (B5-3) claims `priority DESC`
    /// higher-first. The pinning property is *precedence*: moderation must be
    /// claimed ahead of message/room backlog regardless of enqueue order.
    /// Do not "align" these two models.
    #[test]
    fn moderation_lane_preempts_backlog_under_desc_claim() {
        let lane =
            governance_lane_for(LOCAL_ACTION_MODERATED).expect("message.moderated is mapped");
        assert_eq!(lane.priority, GOVERNANCE_PRIORITY_MODERATION);
        // DESC: higher = claimed first. Moderation (100) preempts backlog (10)
        // exactly like priority_for(Moderate)=10 preempts Embed=100 in the
        // ai_jobs ASC model — same precedence, opposite numeric direction.
        // (Compared against the runtime lane value so the pin survives a
        // "direction fix" without tripping clippy's const-assert lint.)
        assert!(
            lane.priority > GOVERNANCE_PRIORITY_BACKLOG,
            "moderation lane must sort ahead of backlog under DESC claim"
        );
    }

    /// R1 pass-through: unknown local tokens map to `None` — the shared 0239
    /// trigger/redirect must never raise or block non-moderation audit rows.
    /// `message.deleted` is included per R-D2 (see the dedicated test).
    /// `message.create`/`message.edit` stay `None` BY DESIGN: L1 aggregation
    /// is a SQL-side allowlist (0242 trigger), not a governance lane — mapping
    /// them here would collide with the R-D2 mis-tokened-rows-stay-out pin.
    /// The room tokens are NOT here: `room.create`/`room.archived` are mapped
    /// to the room lane since migration 0245 (see
    /// `room_lane_maps_to_room_class`) — they must NOT pass through unmapped.
    #[test]
    fn unknown_local_token_passes_through_unmapped() {
        for token in [
            LOCAL_ACTION_MESSAGE_CREATE,
            LOCAL_ACTION_MESSAGE_EDIT,
            LOCAL_ACTION_MESSAGE_RECALLED,
            LOCAL_ACTION_MESSAGE_DELETED,
            "call.join",
            "",
        ] {
            assert_eq!(
                governance_lane_for(token),
                None,
                "token {token:?} must pass through unmapped"
            );
            assert!(!is_admin_class(token), "token {token:?} is not admin-class");
        }
    }

    /// R-D2 pin (security-review closeout): the pass-through negative is
    /// deliberate, not incidental. A moderation finalize mis-tokened as
    /// `message.deleted` (user-delete) must NOT enter the admin lane — it
    /// would be audited (looks fine), produce no outbound token, and be
    /// merged by L1's high-volume `message.*` window. A future change routing
    /// `message.deleted` into the admin lane, or turning pass-through into a
    /// raise (second abort path), fails this test.
    #[test]
    fn user_delete_token_stays_out_of_admin_lane() {
        assert_eq!(governance_lane_for(LOCAL_ACTION_MESSAGE_DELETED), None);
        assert!(!is_admin_class(LOCAL_ACTION_MESSAGE_DELETED));
    }

    /// R1 token-keyed, not caller-keyed: the mapping is a pure function of the
    /// token — one match arm, no identity input — so every `message.moderated`
    /// producer (`worker`, `moderate_delete`/`moderation_bot`, report review with
    /// actor=Some(reviewer)) stamps identically. `message.moderated` is the
    /// only admin-class token today.
    #[test]
    fn mapping_is_token_keyed() {
        let a = governance_lane_for(LOCAL_ACTION_MODERATED).expect("mapped");
        let b = governance_lane_for("message.moderated").expect("mapped");
        assert_eq!(a, b, "same token ⇒ same tuple, regardless of caller");
        // No other in-repo local action token is admin-class (the L1
        // allowlist tokens included — they stay unmapped, see the L1 design).
        for token in [
            LOCAL_ACTION_MESSAGE_DELETED,
            LOCAL_ACTION_MESSAGE_CREATE,
            LOCAL_ACTION_MESSAGE_RECALLED,
            LOCAL_ACTION_ROOM_CREATE,
            LOCAL_ACTION_ROOM_ARCHIVED,
        ] {
            assert!(!is_admin_class(token), "{token} must not be admin-class");
        }
        assert!(is_admin_class("message.moderated"));
    }

    /// R1 outbound contract: the mapped tuple carries the single locked
    /// outbound token (see `aero_common::model::audit::MODERATION_OUTBOUND_ACTION`),
    /// the admin class, and the enqueue-time status 0 — the drill fixture's
    /// start state. A contract flip of the outbound token is a one-line leaf
    /// edit and this test follows automatically.
    #[test]
    fn outbound_action_is_single_contract_token() {
        let lane = governance_lane_for(LOCAL_ACTION_MODERATED).expect("mapped");
        assert_eq!(lane.outbound_action, MODERATION_OUTBOUND_ACTION);
        assert_eq!(lane.class, GOVERNANCE_CLASS_ADMIN);
        assert_eq!(lane.status, 0, "status 0 = enqueue-time normative state");
        // Canonical-value pin lives at the leaf (model/audit.rs
        // `vocabulary_consts_are_pinned`); this test pins the mapping's
        // use of the re-exported token, not a second literal.
    }

    /// R5 classification authority: admin-class moderation rows must stay 1:1
    /// (`event_id` = `audit_events.id`), never merged by the landed L1
    /// high-volume `message.*` aggregation window (migration 0242). This is
    /// the classification the 0242 trigger allowlist keys off.
    #[test]
    fn admin_class_rows_never_aggregated() {
        assert!(is_admin_class(LOCAL_ACTION_MODERATED));
        // Message/room backlog classes are the aggregatable population.
        assert!(!is_admin_class(LOCAL_ACTION_MESSAGE_CREATE));
        assert!(!is_admin_class(LOCAL_ACTION_MESSAGE_RECALLED));
        assert!(!is_admin_class(LOCAL_ACTION_ROOM_CREATE));
    }

    /// A2.1/A2.2 pin — migration 0245 room lane (B5-1 completion): the
    /// room-family tokens map to the room class (1:1, backlog priority, local
    /// token verbatim), never admin, never None. This is the Rust-side twin
    /// of the 0245 trigger's SQL allowlist (cross-pinned by the aero-storage
    /// `room_lane_outbox_parity` `db_test` recomputed from the leaf consts).
    #[test]
    fn room_lane_maps_to_room_class() {
        for token in [LOCAL_ACTION_ROOM_CREATE, LOCAL_ACTION_ROOM_ARCHIVED] {
            let lane = governance_lane_for(token).expect("room tokens are mapped since 0245");
            assert_eq!(lane.class, GOVERNANCE_CLASS_ROOM, "{token} → room class");
            assert_eq!(
                lane.priority, GOVERNANCE_PRIORITY_BACKLOG,
                "{token} → backlog priority 10 (DESC: claimed after moderation)"
            );
            assert_eq!(
                lane.outbound_action, token,
                "{token} outbound action = local token verbatim (no fabricated contract token)"
            );
            assert_eq!(lane.status, 0, "status 0 = enqueue-time normative state");
            assert!(
                !is_admin_class(token),
                "{token} is room-class, never admin-class"
            );
        }
    }

    /// Plain mirror of the storage `moderation_finalize_outbox_parity` drill:
    /// keep both the leaf constants and the DDL-facing literal tuple pinned.
    #[test]
    fn finalize_contract_mirrors_drill_fixture() {
        let job = moderation_job(Some(Uuid::new_v4()));
        let contract = finalize_contract_for(&job, Some("spam")).expect("blocking target");
        let actual = (
            contract.audit_action,
            contract.class,
            contract.priority,
            contract.outbound_action,
            contract.status,
        );
        assert_eq!(
            actual,
            (
                "message.moderated",
                "admin",
                100_i16,
                "admin.content.flag",
                0_i16
            )
        );
        assert_eq!(
            actual,
            (
                LOCAL_ACTION_MODERATED,
                GOVERNANCE_CLASS_ADMIN,
                GOVERNANCE_PRIORITY_MODERATION,
                MODERATION_OUTBOUND_ACTION,
                0_i16
            )
        );
    }

    #[test]
    fn finalize_contract_safe_verdict_yields_no_contract() {
        let job = moderation_job(Some(Uuid::new_v4()));
        assert_eq!(finalize_contract_for(&job, None), None);
    }

    #[test]
    fn finalize_contract_no_target_yields_no_contract() {
        let job = moderation_job(None);
        assert_eq!(finalize_contract_for(&job, Some("spam")), None);
        // Workspace resolution is the separate R-D1 caller gate, not part of
        // this pure tuple mirror.
        let no_workspace = moderation_job(Some(Uuid::new_v4()));
        assert!(finalize_contract_for(&no_workspace, Some("spam")).is_some());
    }

    #[test]
    fn admin_lane_vocabulary_fully_mapped_or_fail_closed() {
        assert!(
            !ADMIN_LANE_TOKENS.is_empty(),
            "admin lane vocabulary must contain at least one authoritative token"
        );
        assert!(
            ADMIN_LANE_TOKENS.contains(&LOCAL_ACTION_MODERATED),
            "authoritative moderation leaf token must enter the admin vocabulary"
        );
        for token in ADMIN_LANE_TOKENS {
            let lane = governance_lane_for(token).expect("admin token must be mapped");
            assert_eq!(lane.class, GOVERNANCE_CLASS_ADMIN);
            assert_eq!(lane.priority, GOVERNANCE_PRIORITY_MODERATION);
            assert_eq!(lane.status, 0);
            let contract =
                finalize_contract_for(&moderation_job(Some(Uuid::new_v4())), Some("blocked"))
                    .expect("mapped admin token has a finalize contract");
            assert_eq!(contract.audit_action, *token);
            assert_eq!(contract.class, lane.class);
            assert_eq!(contract.priority, lane.priority);
            assert_eq!(contract.outbound_action, lane.outbound_action);
            assert_eq!(contract.status, lane.status);
        }
        assert!(!ADMIN_LANE_TOKENS.contains(&LOCAL_ACTION_MESSAGE_DELETED));
        assert!(!ADMIN_LANE_TOKENS.contains(&"message.deleted"));
    }
}
