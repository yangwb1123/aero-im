//! `audit_governance_outbox` (v2) contract fixtures — PG `db_tests` for the
//! 0239 migration landed by the B5-1 migration slice.
//!
//! Harness contract (`scripts/test-integration.sh` `run_migrated_integration`
//! entries, gated on `migrations/0239_audit_governance_outbox.sql`):
//!   * filter `audit_governance::` — this module's `db_tests` (the harness
//!     empty-filter guard requires ≥1 matched test; a vacuous green is a
//!     FAIL, never a pass);
//!   * filter `moderation_finalize_outbox_parity` — the named parity test
//!     below, the **in-tx oracle** for the additive 0239 trigger (F9: the
//!     relay drills seed the table directly and cannot catch a trigger that
//!     never fires).
//!
//! Cross-slice pins: every literal asserted here mirrors
//! `aero_common::model::audit` (the leaf single source: `GOVERNANCE_CLASS_ADMIN`,
//! `MODERATION_OUTBOUND_ACTION`, `LOCAL_ACTION_MODERATED`),
//! `aero_ai::governance::GOVERNANCE_PRIORITY_MODERATION = 100`,
//! `GOVERNANCE_PRIORITY_BACKLOG = 10`, and the 0239 SQL literals verbatim.
//! aero-storage must not depend on aero-ai (dependency direction), so the
//! leaf constants + these behavioral tests are the drift guards.
//!
//! The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a
//! sibling slice (handoff H3 of the 0239 design doc); this module carries the
//! DDL-contract fixtures only — add the repo to this same module when it
//! lands, keeping the parity test's name and literals stable (it is now a
//! pinned contract fixture).

#[cfg(test)]
mod db_tests {
    use aero_common::{
        AuditActor, AuditClaimPayload, AuditId, AuditTarget, MessageId, ParticipantId, RoomId,
        WorkspaceId, MODERATION_OUTBOUND_ACTION, AGGREGATED_MESSAGE_ACTION, AUDIT_AGGREGATE_TYPE,
        AUDIT_ACTOR_TYPE_PARTICIPANT, AUDIT_DATA_CLASSIFICATION, AUDIT_EVENT_TYPE,
        AUDIT_OUTCOME_SUCCESS, AUDIT_RETENTION_CLASS, AUDIT_SCHEMA_ID, AUDIT_SCHEMA_VERSION,
        AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, L1_WINDOW_SECONDS,
        LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_EDIT, LOCAL_ACTION_MESSAGE_RECALLED,
        LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE,
    };
    use sqlx::PgPool;
    use uuid::Uuid;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Throwaway workspace + actor, self-contained (mirrors audit.rs db_tests).
    async fn fixture(repo_pool: &PgPool) -> (WorkspaceId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("gov-actor-{actor}"))
            .execute(repo_pool)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        let mut tx = repo_pool.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(ws.to_uuid())
        .bind("Governance Test WS")
        .bind(format!("gov-{ws}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        (ws, actor)
    }

    // Room + message fixture in `ws`, so the transactional delete+audit tests
    // have something real to soft-delete (mirrors audit.rs db_tests).
    async fn message_in_workspace(p: &PgPool, ws: WorkspaceId, sender: ParticipantId) -> MessageId {
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(room.to_uuid())
        .bind(format!("gov-tx-room-{room}"))
        .bind(sender.to_uuid())
        .bind(ws.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let repo = crate::message::MessageRepo::new(p.clone());
        repo.insert(crate::message::NewMessage {
            room_id: room,
            sender_id: sender,
            blocks: vec![aero_common::Block::text(
                "to be deleted, audited, governance-enqueued atomically",
            )],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert message")
        .id
    }

    // Gate 1 + Gate 2 prerequisites for the 0239 trigger (and the 0236 v1
    // trigger): commercial enforcement enabled + an enabled binding for the
    // workspace. Same setup the A1 spec and harness leg D use. Tenant/client/
    // source are globally UNIQUE (0235) — suffix with the ws uuid so parallel
    // tests on one DB cannot collide. Also seeds an active entitlement
    // projection (0235 `messages_snaplink_metering` fires on message INSERT
    // when enforcement is on and would otherwise raise
    // 'commercial entitlement is unavailable').
    async fn enable_enforcement_with_binding(p: &PgPool, ws: WorkspaceId) {
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = TRUE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(p)
        .await
        .expect("enable commercial enforcement");
        sqlx::query(
            "INSERT INTO snaplink_commercial_bindings
                   (workspace_id, tenant_id, client_id, audit_client_id, source_system,
                    revision, enabled)
             VALUES ($1, $2, $3, $4, $5, 1, TRUE)
             ON CONFLICT (workspace_id) DO NOTHING",
        )
        .bind(ws.to_uuid())
        .bind(format!("tenant-{ws}"))
        .bind(format!("client-{ws}"))
        .bind(format!("audit-client-{ws}"))
        .bind(format!("source-{ws}"))
        .execute(p)
        .await
        .expect("seed enabled binding");
        sqlx::query(
            "INSERT INTO snaplink_entitlement_projections
                   (workspace_id, tenant_id, revision, active, im_enabled,
                    notifications_enabled, messages_soft, messages_hard,
                    messages_unlimited, notifications_soft, notifications_hard,
                    notifications_unlimited, effective_at, generated_at)
             VALUES ($1, $2, 1, TRUE, TRUE, TRUE, 0, 0, TRUE, 0, 0, TRUE,
                     clock_timestamp(), clock_timestamp())
             ON CONFLICT (workspace_id) DO NOTHING",
        )
        .bind(ws.to_uuid())
        .bind(format!("tenant-{ws}"))
        .execute(p)
        .await
        .expect("seed active entitlement projection");
    }

    /// Restore the fresh-DB default (`snaplink_commercial_runtime.enabled =
    /// FALSE`, 0235) after a test that flipped it on. The singleton is
    /// GLOBAL: the harness's main integration leg runs the whole ignored
    /// suite on ONE shared DB, so a test that leaves the switch on makes
    /// every later message INSERT (through the 0235 metering trigger) raise
    /// P0001 in workspaces without bindings — restore-at-end keeps the
    /// module order-independent (mirror of the defensive re-asserts at
    /// start in the disabled-window tests).
    async fn restore_enforcement_disabled(p: &PgPool) {
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(p)
        .await
        .expect("restore enforcement disabled (fresh-DB default)");
    }

    // The exact production seam `AiWorker::handle_moderate` calls
    // (worker/mod.rs): system soft-delete + `message.moderated` audit append
    // in ONE transaction (aero-storage events.rs `soft_delete_outboxed_system`).
    async fn moderate_finalize(
        p: &PgPool,
        id: MessageId,
        ws: WorkspaceId,
        detail: serde_json::Value,
    ) -> Result<Option<crate::message::events::OutboxedMessageDelete>, sqlx::Error> {
        crate::message::MessageRepo::new(p.clone())
            .soft_delete_outboxed_system(
                id,
                Some(ws),
                None,
                Some("message.moderated"),
                detail,
                ParticipantId::nil(),
                None,
            )
            .await
    }

    async fn count_governance_rows(p: &PgPool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM audit_governance_outbox")
            .fetch_one(p)
            .await
            .expect("count governance rows")
            .0
    }

    // Workspace-scoped outbox count: the harness entry shares ONE throwaway
    // DB across all `audit_governance::` tests, and an earlier test (parity)
    // may leave an enabled-binding workspace with its own moderation audit
    // row — the reconciler correctly backfills that too. Tests whose
    // assertions are about their own fixtures filter on the envelope's
    // `aggregate_id` (== workspace id), never on the global table.
    async fn governance_rows_for(p: &PgPool, ws: WorkspaceId) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(p)
        .await
        .expect("count workspace-scoped governance rows")
    }

    // Order-independence within one harness entry run (tests share the DB;
    // the harness grants each entry its own throwaway DB, but not each test).
    // Full-table counts/selects below assume a pristine table, so each test
    // resets it at start — same isolation assumption the drills document.
    async fn reset_governance_table(p: &PgPool) {
        sqlx::query("DELETE FROM audit_governance_outbox")
            .execute(p)
            .await
            .expect("reset governance table");
    }

    /// A1 parity (the harness-named entry): one AI-moderated message → one
    /// `message.moderated` audit row + one mapped governance row, same tx;
    /// rollback removes both; replay is an idempotent no-op. Pins the 0239
    /// DDL contract halves (1)-(9): `event_id` 1:1, status 0, class 'admin',
    /// priority 100, payload action = the leaf
    /// `MODERATION_OUTBOUND_ACTION`, `UNIQUE(event_id)` dedup, v1
    /// coexistence, fail-closed missing-binding abort.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn moderation_finalize_outbox_parity() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        // Enforcement + binding BEFORE the message insert: the 0235
        // `messages_snaplink_metering` trigger fires on message INSERT once
        // the global singleton is enabled, and would raise without a binding
        // + active entitlement for this workspace.
        enable_enforcement_with_binding(&p, ws).await;
        let id = message_in_workspace(&p, ws, actor).await;

        // --- Commit half (A1-C) ---
        let deleted = moderate_finalize(
            &p,
            id,
            ws,
            serde_json::json!({ "reason": "spam", "digest": "buy now" }),
        )
        .await
        .expect("moderation finalize commits");
        assert!(deleted.is_some(), "first moderation finalize deletes");

        // Half 1: message soft-deleted, blocks cleared.
        let after = crate::message::MessageRepo::new(p.clone())
            .get(id)
            .await
            .unwrap()
            .expect("row still exists");
        assert!(after.deleted_at.is_some(), "message is soft-deleted");
        assert!(after.blocks.is_empty(), "blocks were cleared");

        // Half 2: exactly one audit row — local token verbatim, system actor.
        let audit: Vec<(String, String, Option<String>, String, serde_json::Value)> =
            sqlx::query_as(
                "SELECT id::text, action, actor_id::text, target, detail
                   FROM audit_events
                  WHERE workspace_id = $1 AND action = 'message.moderated'",
            )
            .bind(ws.to_uuid())
            .fetch_all(&p)
            .await
            .expect("query audit rows");
        assert_eq!(audit.len(), 1, "exactly one message.moderated audit row");
        assert_eq!(
            audit[0].1, "message.moderated",
            "local token stays verbatim"
        );
        assert_eq!(audit[0].2, None, "system-initiated → actor_id NULL");
        assert_eq!(audit[0].3, id.to_string(), "target = message id");
        assert_eq!(audit[0].4["reason"], "spam");
        assert_eq!(audit[0].4["digest"], "buy now");

        // Half 3: exactly one governance row — mapped fields + event_id 1:1.
        let gov: Vec<(String, i32, String, i16, serde_json::Value)> = sqlx::query_as(
            "SELECT event_id::text, status, class, priority, payload
               FROM audit_governance_outbox",
        )
        .fetch_all(&p)
        .await
        .expect("query governance rows");
        assert_eq!(gov.len(), 1, "exactly one governance row");
        assert_eq!(
            gov[0].0, audit[0].0,
            "event_id 1:1 with audit_events.id (P2 parity)"
        );
        assert_eq!(gov[0].1, 0, "status 0 = enqueued (0239 normative)");
        assert_eq!(
            gov[0].2, "admin",
            "class 'admin' (GOVERNANCE_CLASS_ADMIN cross-slice pin)"
        );
        assert_eq!(
            gov[0].3, 100,
            "priority 100 (GOVERNANCE_PRIORITY_MODERATION cross-slice pin)"
        );
        assert_eq!(
            gov[0].4["action"], MODERATION_OUTBOUND_ACTION,
            "payload action = leaf MODERATION_OUTBOUND_ACTION (A2 half 5)"
        );
        assert_eq!(gov[0].4["event_id"], audit[0].0, "payload event_id mirrors");
        assert_eq!(gov[0].4["source_system"], format!("source-{ws}"));

        // Half 4 (A1-C(4)): the v1 row still flows (0236 trigger untouched) —
        // v1/v2 coexistence during the cutover window.
        let v1: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
        )
        .bind(&audit[0].0)
        .fetch_one(&p)
        .await
        .expect("count v1 rows")
        .0;
        assert_eq!(
            v1, 1,
            "v1 audit delivery row coexists (unmapped local token)"
        );
        let v1_action: String = sqlx::query_scalar(
            "SELECT payload->>'action' FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
        )
        .bind(&audit[0].0)
        .fetch_one(&p)
        .await
        .expect("v1 payload action");
        assert_eq!(v1_action, "message.moderated", "v1 keeps the local token");

        // --- Rollback half (A1-RB): missing binding → Gate 2 RAISE aborts
        // the whole tx (soft delete + audit + governance row together). ---
        let id2 = message_in_workspace(&p, ws, actor).await;
        sqlx::query("DELETE FROM snaplink_commercial_bindings WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .expect("drop binding");
        let err = moderate_finalize(&p, id2, ws, serde_json::json!({ "reason": "spam" })).await;
        assert!(
            err.is_err(),
            "moderation finalize without a binding must abort (fail-closed)"
        );
        let after2 = crate::message::MessageRepo::new(p.clone())
            .get(id2)
            .await
            .unwrap()
            .expect("row still exists");
        assert!(
            after2.deleted_at.is_none(),
            "soft-delete rolled back with the governance enqueue"
        );
        assert!(!after2.blocks.is_empty(), "blocks were not cleared");
        let orphaned: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE target = $1 AND action = 'message.moderated'",
        )
        .bind(id2.to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(orphaned, 0, "no audit row escaped the rolled-back tx");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "no governance row escaped the rolled-back tx"
        );

        // --- Replay half (A1-RP): repeat of the committed finalize is a
        // no-op — still exactly 1 audit + 1 governance row (R6). ---
        enable_enforcement_with_binding(&p, ws).await;
        let again = moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
            .await
            .expect("replay of a committed finalize returns Ok");
        assert!(
            again.is_none(),
            "replay of a committed finalize is Ok(None)"
        );
        let audit_after: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
        )
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(audit_after, 1, "no second audit row on replay");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "no second governance row on replay"
        );
        // Restore the fresh-DB default: the singleton is global and this
        // entry's DB may be shared (main integration leg runs the whole
        // ignored suite on one DB) — leaving the switch on would fail every
        // later message-inserting test via the 0235 metering trigger.
        restore_enforcement_disabled(&p).await;
    }

    /// R4 parity drill for the leaf `AuditClaimPayload` — the Rust twin of
    /// the 0239 SQL-built wire envelope, for `message.*`/`room.*` producers
    /// (the trigger only maps `message.moderated`; every other action passes
    /// through unmapped and would otherwise drift with no compile-time pin).
    ///
    /// Half A: the typed struct parses the trigger's ACTUAL row (admin
    /// class) fail-closed and re-serializes Value-equal to the stored JSONB.
    /// (Literal `payload::text == to_string(struct)` byte-equality is
    /// impossible: PG jsonb emits keys in length-then-bytewise canonical
    /// order, which is neither the 0239 document order nor serde's — the
    /// design resolution is Value-level + wire-text-parse equality, which
    /// still fails on every semantic drift: key rename, key addition, value
    /// spelling, `schema_version` as `"1"`.) The mutation-sanity row below
    /// proves the equality is not vacuous.
    ///
    /// Half B: a Rust-produced `message.deleted` row via the real in-tx
    /// producer seam (`AuditRepo::append_in_tx`, `audit.rs:127`, used by
    /// `message/crud.rs:374`), with the governance outbox INSERT in the SAME
    /// transaction; the row is read back and asserted field-by-field against
    /// the 0239-spelled contract. `occurred_at` is derived from
    /// `to_jsonb(created_at)` (PG timestamptz→jsonb spelling — never from a
    /// Rust formatter), and `class`/`priority` are outbox COLUMNS (10 =
    /// `GOVERNANCE_PRIORITY_BACKLOG`, comment-pinned — aero-storage must not
    /// import aero-ai, and the leaf struct intentionally does not hold them).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn rust_produced_payload_matches_0239_envelope() {
        let p = pool();
        reset_governance_table(&p).await;

        // ---- Half A: the 0239 trigger's real row (admin class) parses and
        // re-serializes Value-equal. ----
        let (ws_a, actor_a) = fixture(&p).await;
        enable_enforcement_with_binding(&p, ws_a).await;
        let id_a = message_in_workspace(&p, ws_a, actor_a).await;
        moderate_finalize(&p, id_a, ws_a, serde_json::json!({ "reason": "spam" }))
            .await
            .expect("moderation finalize commits");
        let (stored_text, stored): (String, serde_json::Value) =
            sqlx::query_as("SELECT payload::text, payload FROM audit_governance_outbox")
                .fetch_one(&p)
                .await
                .expect("trigger row payload");
        // Fail-closed parse of BOTH the raw wire text and the JSONB value.
        let parsed: AuditClaimPayload = serde_json::from_str(&stored_text)
            .expect("typed twin parses the trigger's wire text (fail-closed)");
        let parsed_from_value: AuditClaimPayload = serde_json::from_value(stored.clone())
            .expect("typed twin parses the trigger's JSONB value (fail-closed)");
        assert_eq!(parsed, parsed_from_value);
        // Value-level equality with the stored JSONB (occurred_at spelling
        // pinned implicitly: PG's timestamptz→jsonb text must round-trip
        // through the String field verbatim).
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            stored,
            "re-serialized twin must equal the stored JSONB (semantic parity)"
        );
        // Mutation-sanity: a renamed key must fail the fail-closed parse —
        // the equality above is not vacuous, and a future SQL-side rename
        // alarms here until the leaf tracks it.
        let mut mutated = serde_json::to_value(&parsed).unwrap();
        let action = mutated["action"].take();
        mutated
            .as_object_mut()
            .unwrap()
            .insert("actionx".to_owned(), action);
        assert!(
            serde_json::from_value::<AuditClaimPayload>(mutated).is_err(),
            "a renamed key must fail the fail-closed parse (drift alarm)"
        );

        // ---- Half B: Rust-produced `message.deleted` row, field-by-field
        // vs the 0239 envelope, same tx as the audit append. ----
        let (ws_b, actor_b) = fixture(&p).await;
        enable_enforcement_with_binding(&p, ws_b).await;
        let id_b = message_in_workspace(&p, ws_b, actor_b).await;
        let source_system: String = sqlx::query_scalar(
            "SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id = $1",
        )
        .bind(ws_b.to_uuid())
        .fetch_one(&p)
        .await
        .expect("binding source_system");
        assert_eq!(source_system, format!("source-{ws_b}"));

        let detail = serde_json::json!({ "reason": "spam", "digest": "buy now" });
        let mut tx = p.begin().await.expect("begin half-B tx");
        let audit_id = crate::AuditRepo::append_in_tx(
            &mut tx,
            ws_b,
            Some(actor_b),
            "message.deleted",
            Some(id_b.to_string().as_str()),
            detail.clone(),
        )
        .await
        .expect("append_in_tx (the message/crud.rs:374 producer seam)");
        // occurred_at spelling: derived from PG, never from a Rust formatter
        // (`#>> '{}'` unwraps the jsonb string literal to its unquoted text).
        let occurred_at: String = sqlx::query_scalar(
            "SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id = $1",
        )
        .bind(audit_id.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("to_jsonb(created_at) spelling");

        let claim = AuditClaimPayload::new(
            audit_id.to_uuid().to_string(),
            source_system.clone(),
            occurred_at,
            AuditActor::participant(actor_b.to_uuid().to_string()),
            vec![AuditTarget::resource(id_b.to_string())],
            ws_b.to_uuid().to_string(),
            "message.deleted".to_owned(),
            detail.clone(),
        );
        // class 'message' + priority 10 are outbox COLUMNS (comment-pinned
        // = GOVERNANCE_CLASS_MESSAGE / GOVERNANCE_PRIORITY_BACKLOG).
        sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, class, priority, payload)
             VALUES ($1, 'message', 10, $2)",
        )
        .bind(audit_id.to_uuid())
        .bind(serde_json::to_value(&claim).expect("serialize claim"))
        .execute(&mut *tx)
        .await
        .expect("0239 CHECKs accept class 'message' / priority 10");

        // Read back through PG: field-by-field vs the 0239-spelled contract.
        let (row_event_id, row_class, row_priority, stored_b): (
            String,
            String,
            i16,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT event_id::text, class, priority, payload
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(audit_id.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("read back half-B row");
        let back: AuditClaimPayload = serde_json::from_value(stored_b.clone())
            .expect("stored payload parses into the typed twin");
        assert_eq!(row_event_id, audit_id.to_uuid().to_string(), "event_id 1:1");
        assert_eq!(
            row_class, "message",
            "class 'message' (GOVERNANCE_CLASS_MESSAGE)"
        );
        assert_eq!(
            row_priority, 10,
            "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)"
        );
        assert_eq!(back.event_id, audit_id.to_uuid().to_string());
        assert_eq!(back.source_system, source_system);
        assert_eq!(back.event_type, "aero.im.security");
        assert_eq!(back.schema_id, "aero.im.security");
        assert_eq!(back.schema_version, 1);
        assert_eq!(
            back.actor,
            AuditActor::participant(actor_b.to_uuid().to_string()),
            "participant actor for a human actor (0239 CASE)"
        );
        assert_eq!(
            back.targets,
            vec![AuditTarget::resource(id_b.to_string())],
            "targets = [{{id: target, type: 'resource'}}] for a non-NULL target"
        );
        assert_eq!(back.aggregate_type, "workspace");
        assert_eq!(back.aggregate_id, ws_b.to_uuid().to_string());
        assert_eq!(back.action, "message.deleted");
        assert_eq!(back.outcome, "success");
        assert_eq!(back.payload, detail);
        assert_eq!(back.data_classification, "confidential");
        assert_eq!(back.retention_class, "security");
        assert_eq!(back.idempotency_key, back.event_id);
        // The round-trip through PG is itself an assertion: stored JSONB ==
        // to_value(struct) (Value-level; key order is jsonb canonicalization).
        assert_eq!(stored_b, serde_json::to_value(&claim).unwrap());

        tx.rollback().await.expect("rollback half-B tx");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "half-B row rolled back with the audit append; only half-A's row remains"
        );

        // ---- Half C: the AUTH token shape (`auth.register` — the real
        // producer seam from registration.rs) mirrors Half B exactly: the
        // 0239 CHECKs accept class 'message' / priority 10 (the auth backlog
        // lane), every envelope field matches the 0239-spelled contract
        // verbatim, and the outbox row rolls back with the audit append. A
        // drifted token spelling or a renamed envelope key fails here
        // (fail-closed drift alarm, same as Half B).
        let (ws_c, actor_c) = fixture(&p).await;
        // Both AFTER INSERT triggers fire on the audit insert: the v1 trigger
        // does an UNCONDITIONAL binding lookup, so ws_c needs its binding
        // before the append (enforcement is still on from Half B).
        enable_enforcement_with_binding(&p, ws_c).await;
        let source_system_c: String = sqlx::query_scalar(
            "SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id = $1",
        )
        .bind(ws_c.to_uuid())
        .fetch_one(&p)
        .await
        .expect("binding source_system");

        let detail_c = serde_json::json!({ "email": "auth@example.test", "user_agent": "half-c" });
        let mut tx = p.begin().await.expect("begin half-C tx");
        let audit_id_c = crate::AuditRepo::append_in_tx(
            &mut tx,
            ws_c,
            Some(actor_c),
            "auth.register",
            Some(actor_c.to_string().as_str()),
            detail_c.clone(),
        )
        .await
        .expect("append_in_tx (the registration.rs:37 producer seam)");
        // occurred_at spelling: derived from PG, never from a Rust formatter.
        let occurred_at_c: String = sqlx::query_scalar(
            "SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id = $1",
        )
        .bind(audit_id_c.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("to_jsonb(created_at) spelling");

        let claim_c = AuditClaimPayload::new(
            audit_id_c.to_uuid().to_string(),
            source_system_c.clone(),
            occurred_at_c,
            AuditActor::participant(actor_c.to_uuid().to_string()),
            vec![AuditTarget::resource(actor_c.to_string())],
            ws_c.to_uuid().to_string(),
            "auth.register".to_owned(),
            detail_c.clone(),
        );
        sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, class, priority, payload)
             VALUES ($1, 'message', 10, $2)",
        )
        .bind(audit_id_c.to_uuid())
        .bind(serde_json::to_value(&claim_c).expect("serialize claim"))
        .execute(&mut *tx)
        .await
        .expect("0239 CHECKs accept the auth-token lane shape (class 'message' / priority 10)");

        let (row_event_id, stored_c): (String, serde_json::Value) = sqlx::query_as(
            "SELECT event_id::text, payload
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(audit_id_c.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("read back half-C row");
        let back_c: AuditClaimPayload = serde_json::from_value(stored_c.clone())
            .expect("stored payload parses into the typed twin");
        assert_eq!(row_event_id, audit_id_c.to_uuid().to_string(), "event_id 1:1");
        assert_eq!(back_c.event_id, audit_id_c.to_uuid().to_string());
        assert_eq!(back_c.source_system, source_system_c);
        assert_eq!(back_c.event_type, "aero.im.security");
        assert_eq!(back_c.schema_id, "aero.im.security");
        assert_eq!(back_c.schema_version, 1);
        assert_eq!(
            back_c.actor,
            AuditActor::participant(actor_c.to_uuid().to_string()),
            "participant actor (0239 CASE)"
        );
        assert_eq!(
            back_c.targets,
            vec![AuditTarget::resource(actor_c.to_string())],
            "targets = [{{id: participant, type: 'resource'}}]"
        );
        assert_eq!(back_c.aggregate_type, "workspace");
        assert_eq!(back_c.aggregate_id, ws_c.to_uuid().to_string());
        assert_eq!(back_c.action, "auth.register", "action verbatim (token pin)");
        assert_eq!(back_c.outcome, "success");
        assert_eq!(back_c.payload, detail_c);
        assert_eq!(back_c.data_classification, "confidential");
        assert_eq!(back_c.retention_class, "security");
        assert_eq!(back_c.idempotency_key, back_c.event_id);
        assert_eq!(stored_c, serde_json::to_value(&claim_c).unwrap());

        tx.rollback().await.expect("rollback half-C tx");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "half-C row rolled back with the audit append; only half-A's row remains"
        );
        // Restore the fresh-DB default (see `restore_enforcement_disabled`).
        restore_enforcement_disabled(&p).await;
    }

    /// DDL contract shape: the 12 consumer-pinned columns with the defaults
    /// the t11/relay drills rely on (they INSERT only
    /// `event_id`/`payload`/`available_at`/`attempts`/`status`), the status CHECK, the
    /// claim-state CHECK, the value CHECKs (finding 4: `class` / `priority` /
    /// `delivery_mode` — a typo'd literal fails at INSERT, never silently), the
    /// `event_id` PK (satisfying the pinned "`UNIQUE(event_id)`" dedup
    /// contract), and the due index matching the claim ORDER BY.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ddl_contract_defaults_and_checks() {
        let p = pool();
        reset_governance_table(&p).await;

        // Drill INSERT shape (no priority/class/delivery_mode) → defaults apply.
        let event_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_governance_outbox
                   (event_id, payload, available_at, attempts, status)
             VALUES ($1, '{\"k\":\"v\"}'::jsonb, clock_timestamp(), 0, 0)",
        )
        .bind(event_id)
        .execute(&p)
        .await
        .expect("drill-shape insert succeeds");
        let row: (i16, String, String, i32, i64) = sqlx::query_as(
            "SELECT priority, class, delivery_mode, status, attempts
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_one(&p)
        .await
        .expect("read back defaults");
        assert_eq!(
            row.0, 10,
            "priority DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG (governance.rs:33)"
        );
        assert_eq!(
            row.1, "message",
            "class DEFAULT 'message' (GOVERNANCE_CLASS_MESSAGE)"
        );
        assert_eq!(row.2, "push", "delivery_mode DEFAULT 'push' (reserved)");
        assert_eq!(row.3, 0, "status DEFAULT 0 (enqueued)");
        assert_eq!(row.4, 0, "attempts DEFAULT 0");

        // PK on event_id (semantically the pinned UNIQUE(event_id) contract).
        let pk: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM pg_constraint
              WHERE conrelid = 'audit_governance_outbox'::regclass AND contype = 'p'",
        )
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(pk, 1, "event_id PRIMARY KEY present");

        // Status CHECK 0..3 (connector status machine, pg.rs:27-30) — asserted
        // behaviorally: 3 is legal (dead), 4 must be rejected by the CHECK.
        sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, status, payload)
             VALUES ($1, 3, '{}'::jsonb)",
        )
        .bind(Uuid::new_v4())
        .execute(&p)
        .await
        .expect("status 3 (dead) is legal");
        let err = sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, status, payload)
             VALUES ($1, 4, '{}'::jsonb)",
        )
        .bind(Uuid::new_v4())
        .execute(&p)
        .await;
        assert!(err.is_err(), "status 4 must be rejected by the CHECK");

        // Value CHECKs (finding 4): typo'd class / non-positive priority /
        // unknown delivery_mode are rejected at INSERT — the low-severity
        // silent-pass gap is closed behaviorally (checked BEFORE the
        // claim-state CHECK is probed so a violated CHECK aborts the INSERT
        // exactly as designed).
        let bad_class = sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, class, payload)
             VALUES ($1, 'administrator', '{}'::jsonb)",
        )
        .bind(Uuid::new_v4())
        .execute(&p)
        .await;
        assert!(
            bad_class.is_err(),
            "class outside the admin/message/room lanes must be rejected (GOVERNANCE_CLASS_*)"
        );
        let bad_priority = sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, priority, payload)
             VALUES ($1, 0, '{}'::jsonb)",
        )
        .bind(Uuid::new_v4())
        .execute(&p)
        .await;
        assert!(
            bad_priority.is_err(),
            "priority 0 must be rejected (DESC lane: higher = claimed first, > 0)"
        );
        let bad_mode = sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, delivery_mode, payload)
             VALUES ($1, 'pull', '{}'::jsonb)",
        )
        .bind(Uuid::new_v4())
        .execute(&p)
        .await;
        assert!(
            bad_mode.is_err(),
            "delivery_mode outside the push lane must be rejected (reserved lane)"
        );
        // The lanes themselves stay legal: 'admin'/'message'/'room' classes and
        // the priority-drill's [PROPOSED] 100/200 values all pass the value
        // CHECKs (the drill seeds them directly).
        for (class, priority) in [("admin", 100_i16), ("message", 10), ("room", 200)] {
            sqlx::query(
                "INSERT INTO audit_governance_outbox (event_id, class, priority, payload)
                 VALUES ($1, $2, $3, '{}'::jsonb)",
            )
            .bind(Uuid::new_v4())
            .bind(class)
            .bind(priority)
            .execute(&p)
            .await
            .expect("documented lane values must pass the value CHECKs");
        }

        let check_defs: Vec<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conrelid = 'audit_governance_outbox'::regclass AND contype = 'c'",
        )
        .fetch_all(&p)
        .await
        .expect("list check constraints");
        assert!(
            check_defs
                .iter()
                .any(|d| d.contains("claim_token") && d.contains("lease_expires_at")),
            "claim-state CHECK (token ⇔ lease) present"
        );

        // Due partial index matches the claim ORDER BY (available_at,
        // created_at, event_id) over the status IN (0,1) window (pg.rs:78-88).
        let index_def: String = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes
              WHERE tablename = 'audit_governance_outbox'
                AND indexname = 'audit_governance_due_idx'",
        )
        .fetch_one(&p)
        .await
        .expect("due index exists");
        assert!(
            index_def.contains("(available_at, created_at, event_id)"),
            "due index column order matches claim ORDER BY"
        );
        assert!(
            index_def.contains("status") && index_def.contains('0') && index_def.contains('1'),
            "due index is partial over the claimable window status IN (0,1) (rendered as status = ANY (ARRAY[0, 1]) on PG 17)"
        );
    }

    /// A2 half 4: enforcement disabled → the audit row still commits, zero
    /// governance rows (the 0239 Gate 1 skip; the commercial kill-switch is
    /// a delivery gate, never an audit-loss gate).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn moderation_finalize_runtime_disabled_commits_1_plus_0() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        // Fresh DB default: snaplink_commercial_runtime.enabled = FALSE (0235).
        // Defensively re-assert the disabled state BEFORE the message insert:
        // the singleton is global and an earlier test in the same DB run may
        // have flipped it (the harness grants each entry its own throwaway DB,
        // but tests within one entry share it). No binding seeded — Gate 1
        // skips before the binding lookup.
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let id = message_in_workspace(&p, ws, actor).await;
        let deleted = moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
            .await
            .expect("moderation finalize commits while disabled");
        assert!(deleted.is_some(), "soft delete commits");
        let audit: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
        )
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(audit, 1, "audit trail commits while enforcement disabled");
        assert_eq!(
            count_governance_rows(&p).await,
            0,
            "|G|=0 while disabled (A2 half 4 pin)"
        );
    }

    /// A2 half 9 (pass-through no-raise): a non-moderation action keeps
    /// flowing through the shared trigger untouched — zero governance rows,
    /// and the v1 row is still produced (0236 trigger, action verbatim).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn non_moderation_action_passes_through_unmapped() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        // Enforcement + binding before the message insert (meter trigger, see
        // the parity test's setup note).
        enable_enforcement_with_binding(&p, ws).await;
        let id = message_in_workspace(&p, ws, actor).await;

        let deleted = crate::message::MessageRepo::new(p.clone())
            .soft_delete_audited(id, ws, Some(actor), serde_json::json!({ "digest": "x" }))
            .await
            .expect("plain delete commits");
        assert!(deleted, "delete commits");

        let action: String = sqlx::query_scalar(
            "SELECT action FROM audit_events WHERE workspace_id = $1 AND target = $2",
        )
        .bind(ws.to_uuid())
        .bind(id.to_string())
        .fetch_one(&p)
        .await
        .expect("audit row exists");
        assert_eq!(action, "message.deleted", "local token flows verbatim");
        assert_eq!(
            count_governance_rows(&p).await,
            0,
            "unmapped token produces zero governance rows (pass-through no-raise)"
        );
        // v1 row for THIS audit row (scoped by idempotency_key = audit id —
        // the table is shared across tests within one entry run).
        let audit_id: String = sqlx::query_scalar(
            "SELECT id::text FROM audit_events WHERE workspace_id = $1 AND target = $2",
        )
        .bind(ws.to_uuid())
        .bind(id.to_string())
        .fetch_one(&p)
        .await
        .expect("audit id");
        let v1: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
        )
        .bind(audit_id)
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(v1, 1, "v1 row still produced for unmapped actions");
        // Restore the fresh-DB default (see `restore_enforcement_disabled`
        // doc): this test is the module's last in alphabetical run order, so
        // without the restore the shared main-integration DB would stay
        // enforcement-enabled and every later message-inserting test raises
        // P0001 (the 0235 metering trigger, binding lookup).
        restore_enforcement_disabled(&p).await;
    }

    /// R5 / A2 pin: AUTH tokens pass through the 0239 trigger unmapped — with
    /// enforcement enabled, an `auth.register` / `auth.login.failed` row
    /// produces ZERO governance outbox rows and never raises (a raise inside
    /// the trigger would abort the WHOLE register transaction — load-bearing
    /// for the in-tx path). The pin MUST insert into the BOUND fixture
    /// workspace, never nil: the 0236 v1 trigger does its binding lookup
    /// unconditionally and `aero_snaplink_binding_for_workspace` raises P0001
    /// on a missing binding — nil raises in the fixture's raw-SQL enforcement
    /// state (finding A2; supported deployments always bind nil at enable,
    /// §3.5 of the design). With the binding present the v1 trigger enqueues
    /// exactly one `snaplink_delivery_outbox` row per auth row (action
    /// verbatim) — asserted, not assumed.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn auth_tokens_pass_through_unmapped_and_enqueue_v1() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        enable_enforcement_with_binding(&p, ws).await;

        // Both producer shapes: the in-tx register seam and the best-effort
        // login seam (out-of-tx is fine here — the triggers fire on the
        // INSERT regardless).
        let audit_register = crate::AuditRepo::new(p.clone())
            .append(
                ws,
                Some(actor),
                "auth.register",
                Some(actor.to_string().as_str()),
                serde_json::json!({ "email": "pin@example.test", "user_agent": "pin" }),
            )
            .await
            .expect("auth.register audit row commits (no raise)");
        let audit_failed = crate::AuditRepo::new(p.clone())
            .append(
                ws,
                None,
                "auth.login.failed",
                None,
                serde_json::json!({ "email": "pin@example.test", "reason": "invalid_credentials" }),
            )
            .await
            .expect("auth.login.failed audit row commits (no raise)");

        assert_eq!(
            count_governance_rows(&p).await,
            0,
            "auth tokens produce zero governance rows (0239 pass-through no-raise)"
        );
        // One v1 row per auth row, scoped by idempotency_key = audit id (the
        // table is shared across tests within one entry run).
        for id in [audit_register, audit_failed] {
            let v1: i64 = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM snaplink_delivery_outbox
                  WHERE destination = 'audit' AND idempotency_key = $1",
            )
            .bind(id.to_uuid().to_string())
            .fetch_one(&p)
            .await
            .unwrap()
            .0;
            assert_eq!(v1, 1, "v1 row still produced for auth tokens");
        }
        // Restore the fresh-DB default (see `restore_enforcement_disabled`).
        restore_enforcement_disabled(&p).await;
    }

    /// F6 / §6.4 dedup: the trigger's `ON CONFLICT (event_id) DO NOTHING`
    /// swallows a second fire for the same audit id (`audit_events`' composite
    /// PK `(id, created_at)` admits a same-id/different-created_at duplicate
    /// at the DB level), and a plain duplicate INSERT raises `unique_violation`
    /// — the pinned `UNIQUE(event_id)` dedup contract the sibling's redirect
    /// must also honor with its own ON CONFLICT (handoff H4).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn duplicate_event_id_is_deduped_by_on_conflict() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, _actor) = fixture(&p).await;
        enable_enforcement_with_binding(&p, ws).await;

        let event_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, action, detail)
             VALUES ($1, $2, 'message.moderated', '{\"reason\":\"spam\"}'::jsonb)",
        )
        .bind(event_id)
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .expect("first audit row");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "first fire enqueues one governance row"
        );

        // Clear the v1 row first: v1's UNIQUE(destination, idempotency_key)
        // would otherwise abort the second fire — that coexistence is the
        // sibling redirect's concern, not this test's.
        sqlx::query(
            "DELETE FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
        )
        .bind(event_id.to_string())
        .execute(&p)
        .await
        .expect("clear v1 row");

        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, action, detail, created_at)
             VALUES ($1, $2, 'message.moderated', '{\"reason\":\"spam\"}'::jsonb,
                     clock_timestamp() + interval '1 second')",
        )
        .bind(event_id)
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .expect("second fire (same id, different created_at)");
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "ON CONFLICT (event_id) DO NOTHING dedups the second fire"
        );

        // And the plain duplicate INSERT raises (pins the dedup contract).
        let err = sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, payload)
             VALUES ($1, '{}'::jsonb)",
        )
        .bind(event_id)
        .execute(&p)
        .await;
        assert!(
            err.is_err(),
            "duplicate event_id must raise unique_violation"
        );
        assert_eq!(
            count_governance_rows(&p).await,
            1,
            "failed duplicate left the first row intact"
        );
        // Restore the fresh-DB default (see `restore_enforcement_disabled`
        // doc): the singleton is global and the entry DB may be shared.
        restore_enforcement_disabled(&p).await;
    }

    /// Security Q1 closure (0241): the disabled-window reconciler. A
    /// `message.moderated` audit row accepted while the runtime switch is off
    /// commits with zero outbox rows (A2 half 4); after re-enablement,
    /// `aero_reconcile_governance_audit` backfills it — enabled-binding join,
    /// NOT EXISTS scan, trigger-identical envelope, idempotent, token-keyed
    /// (an unmapped `message.deleted` row is NEVER fabricated into the admin
    /// lane), dead rows never resurrected. Parity converges to
    /// COUNT(outbox) == COUNT(audit) over the workspace's mapped subset
    /// (assertions are ws-scoped: the entry DB is shared with the parity
    /// test, whose enabled-binding workspace is legitimately backfilled too).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn governance_reconcile_backfills_disabled_window() {
        let p = pool();
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        // Isolate from earlier tests in the same entry (they share the
        // throwaway DB; their audit rows persist while the outbox reset above
        // cleared only outbox rows): the reconciler is a GLOBAL scan (v1
        // mirror), so other workspaces' `message.moderated` rows with enabled
        // bindings would be backfilled too and make the returned count
        // non-deterministic. Deleting other workspaces' moderation rows makes
        // the count exact; later tests query only their own workspace.
        sqlx::query(
            "DELETE FROM audit_events
              WHERE action = 'message.moderated' AND workspace_id <> $1",
        )
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .expect("isolate reconcile scan to this workspace");
        // Window open: enforcement disabled (fresh-DB default; defensively
        // re-assert — the singleton is global and an earlier test in the same
        // entry may have flipped it). No binding seeded — Gate 1 skips before
        // the binding lookup.
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");

        // Moderated message commits during the window: audit row yes, outbox
        // row no (the fail-open gate meaning, A2 half 4).
        let id = message_in_workspace(&p, ws, actor).await;
        let deleted = moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
            .await
            .expect("moderation finalize commits while disabled");
        assert!(deleted.is_some(), "soft delete commits");
        assert_eq!(
            count_governance_rows(&p).await,
            0,
            "|G|=0 while disabled (A2 half 4 pin)"
        );

        // A NON-moderation audit row commits in the same window — the
        // reconciler must not fabricate a MODERATION_OUTBOUND_ACTION claim
        // for it (token-keyed R-D2; it stays locally audited + v1-laned only).
        let id2 = message_in_workspace(&p, ws, actor).await;
        crate::message::MessageRepo::new(p.clone())
            .soft_delete_audited(id2, ws, Some(actor), serde_json::json!({ "digest": "x" }))
            .await
            .expect("plain delete commits while disabled");

        // Window closed: re-enable + binding = the recovery precondition
        // (same setup the parity test uses).
        enable_enforcement_with_binding(&p, ws).await;

        // Backfill: the workspace's moderation row is backfilled (the global
        // return value may include earlier tests' enabled-binding workspaces
        // on the shared entry DB — the reconciler must backfill those too,
        // so the pin is ws-scoped). Second run is a no-op (idempotent).
        let backfilled: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
            .fetch_one(&p)
            .await
            .expect("reconcile");
        assert!(
            backfilled >= 1,
            "the disabled-window moderation row must be backfilled (got {backfilled})"
        );
        assert_eq!(
            governance_rows_for(&p, ws).await,
            1,
            "exactly one governance row for THIS workspace; the unmapped action is never \
             fabricated into the admin lane (token-keyed)"
        );
        let again: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
            .fetch_one(&p)
            .await
            .expect("reconcile idempotent");
        assert_eq!(again, 0, "second reconcile inserts nothing");

        // Envelope is byte-identical to the trigger path (A2 field asserts
        // hold for both): event_id 1:1, status 0, class 'admin', priority
        // 100, action = leaf MODERATION_OUTBOUND_ACTION, binding
        // source_system, system actor, idempotency_key = event_id.
        let audit: (String, String, Option<String>, String, serde_json::Value) = sqlx::query_as(
            "SELECT id::text, action, actor_id::text, target, detail
               FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
        )
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("audit row");
        assert_eq!(audit.1, "message.moderated", "local token verbatim");
        let gov: (String, i32, String, i16, serde_json::Value) = sqlx::query_as(
            "SELECT event_id::text, status, class, priority, payload
               FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .expect("backfilled governance row");
        assert_eq!(gov.0, audit.0, "event_id 1:1 with audit_events.id");
        assert_eq!(gov.1, 0, "status 0 = enqueued, same as trigger path");
        assert_eq!(gov.2, "admin", "class 'admin' (GOVERNANCE_CLASS_ADMIN)");
        assert_eq!(gov.3, 100, "priority 100 (GOVERNANCE_PRIORITY_MODERATION)");
        assert_eq!(
            gov.4["action"], MODERATION_OUTBOUND_ACTION,
            "payload action"
        );
        assert_eq!(gov.4["event_id"], audit.0, "payload event_id mirrors");
        assert_eq!(gov.4["idempotency_key"], audit.0, "sink Idempotency-Key");
        assert_eq!(gov.4["source_system"], format!("source-{ws}"));
        assert_eq!(
            gov.4["actor"]["type"], "system",
            "system-initiated moderation"
        );

        // Parity over the mapped subset (enabled window): exactly one outbox
        // row per message.moderated audit row — dead rows still count (parity
        // is row existence, never delivery status).
        let audit_count: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
        )
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(
            governance_rows_for(&p, ws).await,
            audit_count,
            "P2 parity after reconcile (over the workspace's mapped subset)"
        );

        // Dead is terminal: a dead row exists in the outbox, so NOT EXISTS
        // skips it — the reconciler never resurrects a relay-terminal row
        // (it cannot fight a 403-outage dead-letter).
        sqlx::query(
            "UPDATE audit_governance_outbox
                SET status = 3, last_error = 'reconcile-test dead'
              WHERE event_id = $1::uuid",
        )
        .bind(&audit.0)
        .execute(&p)
        .await
        .expect("mark dead");
        let resurrected: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
            .fetch_one(&p)
            .await
            .expect("reconcile after dead");
        assert_eq!(resurrected, 0, "dead row never resurrected");
        let status: i32 = sqlx::query_scalar(
            "SELECT status FROM audit_governance_outbox WHERE event_id = $1::uuid",
        )
        .bind(&audit.0)
        .fetch_one(&p)
        .await
        .expect("dead row status");
        assert_eq!(status, 3, "dead stays dead");

        // Restore the fresh-DB default LAST: the singleton is GLOBAL and this
        // entry's DB may be shared (the main integration leg runs the whole
        // ignored suite on one DB). Leaving the switch on makes every later
        // unbound message INSERT raise P0001 (0235 metering) — empirically
        // `message_reports::db_tests` 3/3 (the F1 hazard, see
        // governance_drill_tests/crash.rs drill_panic_leaves_enforcement_on_*).
        restore_enforcement_disabled(&p).await;
    }

    // ---- L1 window aggregation (migration 0242) — AC2 / AC3 ----------------
    //
    // Harness contract (`scripts/test-integration.sh`): the 0242 file gate
    // runs the `l1_window_aggregates_` filter on its own throwaway DB (the
    // empty-filter guard requires ≥1 matched test). Every literal below is
    // derived from the leaf consts — never inline (G2 closure: truth-check
    // scans `crates --glob '*.rs'` only, so the 0242 SQL side is unpoliced;
    // these tests ARE the SQL pin: a drift in the trigger's allowlist token,
    // class literal, or window divisor breaks the recomputed-key equality).

    /// 0242 presence probe (drill exit-2 precedent): on a partially-migrated
    /// shared DB the trigger is absent → explicit SKIP, never a silent green
    /// (the harness leg is 0242-file-gated anyway, so this only fires on a
    /// shared-DB run).
    async fn l1_aggregate_migrated(p: &PgPool) -> bool {
        let probe: Option<String> = sqlx::query_scalar(
            "SELECT to_regprocedure('aero_enqueue_l1_aggregate_audit()')::text",
        )
        .fetch_one(p)
        .await
        .expect("probe for the 0242 function");
        if probe.is_none() {
            eprintln!("SKIP: 0242 not migrated (aero_enqueue_l1_aggregate_audit missing)");
            return false;
        }
        true
    }

    /// Direct audit_events INSERT through the 0242 trigger (fixture shape:
    /// fresh id, workspace, actor, action, detail, fixed created_at). The
    /// 0236 v1 trigger also fires per row — enforcement must stay off
    /// (Gate 1 fail-open) or the binding lookup runs; the L1 tests never
    /// need a binding.
    async fn insert_audit_row(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ws: WorkspaceId,
        actor: ParticipantId,
        action: &str,
        created_at: time::OffsetDateTime,
    ) {
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .bind(action)
        .bind(created_at)
        .execute(&mut **tx)
        .await
        .expect("insert audit row");
    }

    /// Window key recomputation from the leaf consts — the G2 pin. `md5` is
    /// computed by PG (same function the 0242 trigger uses), so the spelling
    /// is byte-identical to the SQL side.
    async fn recompute_window_key(
        p: &PgPool,
        ws: WorkspaceId,
        created_at: time::OffsetDateTime,
    ) -> Uuid {
        let window_epoch: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint",
        )
        .bind(created_at)
        .bind(L1_WINDOW_SECONDS)
        .fetch_one(p)
        .await
        .expect("window epoch");
        let key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
            .bind(format!(
                "{}|{}|{}",
                ws.to_uuid(),
                GOVERNANCE_CLASS_MESSAGE,
                window_epoch
            ))
            .fetch_one(p)
            .await
            .expect("recompute window key");
        key
    }

    /// AC2 — L1 aggregation is O(windows) not O(N): 5 `message.create` + 1
    /// `message.edit` audit rows in one tx (actions bound from the leaf
    /// consts — the edit row pins the second allowlist token) aggregate into
    /// EXACTLY ONE outbox row whose key is md5(ws|class|window) recomputed
    /// from `GOVERNANCE_CLASS_MESSAGE` + `L1_WINDOW_SECONDS`; the arbitrated
    /// envelope is asserted field-by-field (own `event_id`, `source_system`,
    /// `aggregated`, `action`, window bounds, absence of forbidden keys);
    /// rollback half → 0 rows; second window → 2nd row; second workspace →
    /// 3rd row.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn l1_window_aggregates_5_rows_to_1_outbox() {
        let p = pool();
        if !l1_aggregate_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        // Direct audit INSERTs flow through the 0236 v1 trigger (Gate 1
        // fail-open) and the 0242 trigger; no binding is needed for either.
        // Defensively re-assert the fresh-DB default (the singleton is
        // global and the harness runs the ignored suite on shared DBs).
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");

        // --- Commit half: 5 create + 1 edit, one tx, same window. ---
        let mut tx = p.begin().await.expect("begin l1 tx");
        for i in 0..5 {
            sqlx::query(
                "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6)",
            )
            .bind(Uuid::new_v4())
            .bind(ws.to_uuid())
            .bind(actor.to_uuid())
            .bind(LOCAL_ACTION_MESSAGE_CREATE)
            .bind(serde_json::json!({ "seq": i }))
            .bind(fixed_ts)
            .execute(&mut *tx)
            .await
            .expect("insert message.create audit row");
        }
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_EDIT)
        .bind(serde_json::json!({ "version": 2 }))
        .bind(fixed_ts)
        .execute(&mut *tx)
        .await
        .expect("insert message.edit audit row");
        tx.commit().await.expect("commit l1 tx");

        // 6 audit rows committed.
        let audit_count: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1
                AND action IN ($2, $3)",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_CREATE)
        .bind(LOCAL_ACTION_MESSAGE_EDIT)
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(audit_count, 6, "6 audit rows committed");

        // Exactly ONE outbox row for the workspace — O(windows) not O(N).
        let rows: Vec<(Uuid, i32, String, i16, serde_json::Value)> = sqlx::query_as(
            "SELECT event_id, status, class, priority, payload
               FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read window rows");
        assert_eq!(rows.len(), 1, "6 rows must aggregate into exactly 1 outbox row");
        assert!(
            count_governance_rows(&p).await < 6,
            "COUNT(outbox) = 1 < 6 (the O(windows) bounded property)"
        );
        let (event_id, status, class, priority, payload) = &rows[0];

        // Key recomputation from the leaf consts (the G2 SQL pin): a drift
        // in the trigger's allowlist token, class literal, or window divisor
        // breaks the equality.
        let expected_key = recompute_window_key(&p, ws, fixed_ts).await;
        assert_eq!(
            *event_id, expected_key,
            "window key must be md5(ws|class|floor(epoch/60)) recomputed from the leaf consts"
        );
        assert_eq!(*status, 0, "status 0 = enqueued");
        assert_eq!(
            *class, GOVERNANCE_CLASS_MESSAGE,
            "class must equal GOVERNANCE_CLASS_MESSAGE"
        );
        assert_eq!(
            *priority, 10,
            "priority 10 = GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import aero-ai)"
        );

        // Envelope field-by-field (arbitrated receipt-contract resolution).
        assert_eq!(
            payload["event_id"], event_id.to_string(),
            "payload event_id = row's own PK (the stub receipt echo source — window rows cannot settle without it)"
        );
        assert_eq!(
            payload["source_system"], AUDIT_SOURCE_SYSTEM,
            "payload source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
        );
        assert_eq!(
            payload["idempotency_key"], event_id.to_string(),
            "idempotency_key = own event_id (sink dedup; never a foreign key)"
        );
        assert_eq!(
            payload["action"], AGGREGATED_MESSAGE_ACTION,
            "envelope action = AGGREGATED_MESSAGE_ACTION"
        );
        assert_eq!(payload["count"], 6, "5 create + 1 edit merged into count 6");
        assert_eq!(
            payload["aggregated"], serde_json::json!(true),
            "top-level aggregated=true (cross-slice parity-exemption key)"
        );
        assert!(payload.get("spill").is_none(), "window rows carry no spill marker");
        // Window bounds recomputed from the consts.
        let start_epoch: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM $1::timestamptz))::bigint",
        )
        .bind(payload["window_start"].as_str().expect("window_start string"))
        .fetch_one(&p)
        .await
        .expect("window_start epoch");
        let window_epoch: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint",
        )
        .bind(fixed_ts)
        .bind(L1_WINDOW_SECONDS)
        .fetch_one(&p)
        .await
        .expect("window epoch");
        assert_eq!(
            start_epoch, window_epoch * L1_WINDOW_SECONDS,
            "window_start must be floor(epoch/60)*60"
        );
        let end_epoch: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM $1::timestamptz))::bigint",
        )
        .bind(payload["window_end"].as_str().expect("window_end string"))
        .fetch_one(&p)
        .await
        .expect("window_end epoch");
        assert_eq!(
            end_epoch,
            start_epoch + L1_WINDOW_SECONDS,
            "window_end = window_start + L1_WINDOW_SECONDS"
        );
        assert_eq!(payload["occurred_at"], payload["window_start"]);
        assert!(payload["first_event_at"].is_string(), "first_event_at present");
        assert_eq!(
            payload["last_event_at"], payload["first_event_at"],
            "one fixed timestamp ⇒ first == last"
        );
        // Forbidden keys (arbitrated envelope — a drifted extra key is the
        // fail-closed drift alarm, mirroring AuditClaimPayload's
        // deny_unknown_fields).
        for key in [
            "window_id", "first_event_id", "last_event_id", "tenant_id", "actor", "targets",
        ] {
            assert!(
                payload.get(key).is_none(),
                "forbidden key {key} must not appear in the 0242 envelope"
            );
        }

        // --- Rollback half: an in-tx insert that rolls back leaves zero
        // outbox rows (trigger writes are in-tx with the audit rows). ---
        let mut tx = p.begin().await.expect("begin rollback tx");
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        tx.rollback().await.expect("rollback l1 tx");
        let after_rollback: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(after_rollback, 1, "rolled-back rows must leave no outbox rows");
        let audit_after: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action IN ($2, $3)",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_CREATE)
        .bind(LOCAL_ACTION_MESSAGE_EDIT)
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(audit_after, 6, "rolled-back audit rows are gone too");

        // --- Second window → second row (window key differs). ---
        let earlier = fixed_ts - time::Duration::seconds(2 * L1_WINDOW_SECONDS);
        let mut tx = p.begin().await.expect("begin second-window tx");
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, earlier).await;
        tx.commit().await.expect("commit second-window tx");
        let ws_rows: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(ws_rows, 2, "a second window must produce a second outbox row");

        // --- Second workspace → third row. ---
        let (ws2, actor2) = fixture(&p).await;
        let mut tx = p.begin().await.expect("begin second-workspace tx");
        insert_audit_row(&mut tx, ws2, actor2, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        tx.commit().await.expect("commit second-workspace tx");
        assert_eq!(
            count_governance_rows(&p).await,
            3,
            "second workspace → third row (window key is per (workspace, class, window))"
        );
    }

    /// F5 pin — concurrent merge serialization (the only artifact exercising
    /// the EvalPlanQual path directly): two parallel txs insert one
    /// `message.create` row each into the SAME window; the loser blocks on
    /// the winner's window-row PK lock and re-evaluates SET/WHERE on the
    /// winner's committed row (READ COMMITTED). Assert: exactly 1 outbox
    /// row, count = 2, status = 0.
    ///
    /// Test-doc pins:
    ///   * READ COMMITTED-only guarantee: the re-evaluation is a READ
    ///     COMMITTED behavior; a pool move to REPEATABLE READ/SERIALIZABLE
    ///     turns the second commit into 40001 → this test goes red loudly
    ///     (and in production the message tx would abort).
    ///   * 10s lock-wait bound: `statement_timeout = '10000'` (db.rs:31)
    ///     bounds the trigger's lock wait on a hot window; a hang here fails
    ///     the 15s wall-clock timeout instead of hanging the harness.
    ///   * Dedicated pool: the shared `pool()` caps at 2 connections; this
    ///     test opens `max_connections(3)` and holds 2 connections in the
    ///     parallel txs so they can never deadlock the pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires live Postgres"]
    async fn l1_window_aggregates_concurrent_merge_serializes() {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL");
        if !l1_aggregate_migrated(&pool).await {
            return;
        }
        reset_governance_table(&pool).await;
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&pool)
        .await
        .expect("re-assert enforcement disabled");
        let (ws, actor) = fixture(&pool).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&pool).await.expect("capture fixed ts");

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let spawn_tx = |pool: PgPool,
                        ws: WorkspaceId,
                        actor: ParticipantId,
                        fixed_ts: time::OffsetDateTime,
                        barrier: std::sync::Arc<tokio::sync::Barrier>| {
            tokio::spawn(async move {
                barrier.wait().await;
                let mut tx = pool.begin().await.expect("begin concurrent tx");
                sqlx::query(
                    "INSERT INTO audit_events (id, workspace_id, actor_id, action, created_at)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(Uuid::new_v4())
                .bind(ws.to_uuid())
                .bind(actor.to_uuid())
                .bind(LOCAL_ACTION_MESSAGE_CREATE)
                .bind(fixed_ts)
                .execute(&mut *tx)
                .await
                .expect("concurrent tx insert");
                tx.commit().await.expect("concurrent tx commit");
            })
        };
        let t1 = spawn_tx(pool.clone(), ws, actor, fixed_ts, std::sync::Arc::clone(&barrier));
        let t2 = spawn_tx(pool.clone(), ws, actor, fixed_ts, std::sync::Arc::clone(&barrier));
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            async { tokio::join!(t1, t2) },
        )
        .await
        .expect("concurrent merges must not hang (15s wall-clock bound over the 10s statement timeout)");
        joined.0.expect("tx1 task");
        joined.1.expect("tx2 task");

        let rows: Vec<(serde_json::Value, i32)> = sqlx::query_as(
            "SELECT payload, status FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&pool)
        .await
        .expect("read merged window row");
        assert_eq!(rows.len(), 1, "two concurrent inserts → exactly one window row");
        assert_eq!(rows[0].1, 0, "window row stays status 0 (enqueued)");
        assert_eq!(
            rows[0].0["count"], 2,
            "both merges must land: count = 2 (READ COMMITTED EvalPlanQual re-evaluation)"
        );
        assert_eq!(rows[0].0["event_id"], rows[0].0["idempotency_key"]);
    }

    /// AC3 runtime pin — admin class never aggregated: N `message.create`
    /// rows build a window row (count = N); a moderation finalize yields
    /// exactly 1 admin 1:1 row (`event_id` = audit id, class 'admin',
    /// priority 100) and the window row stays untouched (count still N, no
    /// second row, no increment); raw `message.moderated`/`message.deleted`/
    /// `room.create`/`auth.login` audit inserts create zero window rows.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn admin_rows_never_merged_into_l1_window() {
        let p = pool();
        if !l1_aggregate_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");

        // N message.create rows, same window → 1 window row, count = N.
        const N: i64 = 3;
        let mut tx = p.begin().await.expect("begin create-window tx");
        for _ in 0..N {
            insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        }
        tx.commit().await.expect("commit create-window tx");
        let message_rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT payload FROM audit_governance_outbox
              WHERE class = 'message' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read message-class rows");
        assert_eq!(message_rows.len(), 1, "N creates → exactly 1 window row");
        assert_eq!(message_rows[0].0["count"], N, "count must equal N");

        // Moderation finalize → admin 1:1 row; the window row untouched.
        enable_enforcement_with_binding(&p, ws).await;
        let id = message_in_workspace(&p, ws, actor).await;
        moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
            .await
            .expect("moderation finalize commits");
        let message_rows_after: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT payload FROM audit_governance_outbox
              WHERE class = 'message' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read message-class rows after moderation");
        assert_eq!(
            message_rows_after.len(),
            1,
            "admin moderation must not create a message-class row"
        );
        assert_eq!(
            message_rows_after[0].0["count"], N,
            "admin moderation must not increment the message window (count stays N)"
        );
        let admin_rows: Vec<(Uuid, i32, String, i16, serde_json::Value)> = sqlx::query_as(
            "SELECT event_id, status, class, priority, payload
               FROM audit_governance_outbox
              WHERE class = 'admin' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read admin rows");
        assert_eq!(admin_rows.len(), 1, "exactly one admin 1:1 row");
        assert_eq!(admin_rows[0].2, "admin", "class 'admin'");
        assert_eq!(admin_rows[0].3, 100, "priority 100 = GOVERNANCE_PRIORITY_MODERATION");

        // Raw non-allowlist inserts → zero new/incremented window rows
        // (token allowlist pass-through: only message.create/edit merge).
        let mut tx = p.begin().await.expect("begin non-allowlist tx");
        for action in [
            "message.moderated", "message.deleted", LOCAL_ACTION_ROOM_CREATE, "auth.login",
        ] {
            insert_audit_row(&mut tx, ws, actor, action, fixed_ts).await;
        }
        tx.commit().await.expect("commit non-allowlist tx");
        let message_rows_final: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT payload FROM audit_governance_outbox
              WHERE class = 'message' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read message-class rows after non-allowlist inserts");
        assert_eq!(
            message_rows_final.len(),
            1,
            "non-allowlist tokens must never create a window row"
        );
        assert_eq!(
            message_rows_final[0].0["count"], N,
            "non-allowlist tokens must never increment the window row"
        );
        // The raw message.moderated row produced its own admin 1:1 row
        // (0239 trigger, enforcement + binding enabled) — still no window.
        let admin_after: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE class = 'admin' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(admin_after, 2, "finalize + raw message.moderated = 2 admin 1:1 rows");
        // Restore the fresh-DB default (see `restore_enforcement_disabled`).
        restore_enforcement_disabled(&p).await;
    }

    /// 0245 presence probe (0242 `l1_aggregate_migrated` precedent): on a
    /// partially-migrated shared DB the trigger is absent → explicit SKIP,
    /// never a silent green (the harness leg is 0245-file-gated anyway, so
    /// this only fires on a shared-DB run).
    async fn room_trigger_migrated(p: &PgPool) -> bool {
        let probe: Option<String> = sqlx::query_scalar(
            "SELECT to_regprocedure('aero_enqueue_room_audit()')::text",
        )
        .fetch_one(p)
        .await
        .expect("probe for the 0245 function");
        if probe.is_none() {
            eprintln!("SKIP: 0245 not migrated (aero_enqueue_room_audit missing)");
            return false;
        }
        true
    }

    /// Direct `audit_events` INSERT through the 0245 trigger, RETURNING the id
    /// (the 1:1 assertion needs the audit id; the 0236 v1 trigger also fires
    /// per row — enforcement state decides whether a v1 row is produced).
    async fn insert_audit_row_returning_id(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ws: WorkspaceId,
        actor: ParticipantId,
        action: &str,
        created_at: time::OffsetDateTime,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .bind(action)
        .bind(created_at)
        .fetch_one(&mut **tx)
        .await
        .expect("insert audit row")
    }

    /// A1.1 — room-lane outbox parity (harness slot `room_lane_outbox_parity`,
    /// migration 0245): N raw `room.create`+`room.archived` audit INSERTs
    /// (mixed, in one tx) → exactly N outbox rows, all status 0 / class
    /// 'room' / priority 10, `event_id` 1:1 with `audit_events.id`, envelope
    /// field-by-field recomputed from the leaf consts (G2 closure: the SQL
    /// literals are unpoliced by truth-check — THIS test IS the pin), no
    /// L1 window keys (1:1 rows stay invisible to the parity SUM side), v1
    /// dual-path rows still produced per row (F-A split: the parity leg runs
    /// under `enable_enforcement_with_binding` — with enforcement disabled
    /// there is NO v1 row, so the dual-path assertion cannot live in the
    /// gate-freedom leg), replay dedup (same-id duplicate `audit_events` INSERT is
    /// admitted by `audit_events`' composite PK but deduped by ON CONFLICT),
    /// rollback half → 0 rows.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn room_lane_outbox_parity() {
        let p = pool();
        if !room_trigger_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");
        // Parity leg prerequisites (0239 Gate 1 + Gate 2 for the v1 trigger).
        enable_enforcement_with_binding(&p, ws).await;

        // --- Commit half: mixed room.create + room.archived, one tx. ---
        let mut tx = p.begin().await.expect("begin room parity tx");
        let mut ids = Vec::new();
        for action in [
            LOCAL_ACTION_ROOM_CREATE,
            LOCAL_ACTION_ROOM_ARCHIVED,
            LOCAL_ACTION_ROOM_CREATE,
        ] {
            let id = insert_audit_row_returning_id(&mut tx, ws, actor, action, fixed_ts).await;
            ids.push((id, action));
        }
        tx.commit().await.expect("commit room parity tx");

        // Exactly N outbox rows, 1:1, all mapped fields pinned.
        let rows: Vec<(String, i32, String, i16, serde_json::Value)> = sqlx::query_as(
            "SELECT event_id::text, status, class, priority, payload
               FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read room rows");
        assert_eq!(
            rows.len(),
            ids.len(),
            "N raw room inserts → exactly N 1:1 outbox rows (no aggregation)"
        );
        let expected_occurred: String =
            sqlx::query_scalar("SELECT (to_jsonb($1::timestamptz))::text")
                .bind(fixed_ts)
                .fetch_one(&p)
                .await
                .expect("PG jsonb spelling of occurred_at");
        let expected_occurred = expected_occurred.trim_matches('"');
        for ((id, action), row) in ids.iter().zip(&rows) {
            assert_eq!(&row.0, &id.to_string(), "event_id 1:1 with audit_events.id");
            assert_eq!(row.1, 0, "status 0 = enqueued (0239 normative)");
            assert_eq!(
                row.2, GOVERNANCE_CLASS_ROOM,
                "class 'room' (GOVERNANCE_CLASS_ROOM cross-slice pin)"
            );
            assert_eq!(
                row.3, 10,
                "priority 10 = GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import aero-ai)"
            );
            // Envelope field-by-field (recomputed from the leaf consts).
            assert_eq!(row.4["event_id"], id.to_string(), "payload event_id mirrors");
            assert_eq!(
                row.4["source_system"], AUDIT_SOURCE_SYSTEM,
                "payload source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
            );
            assert_eq!(row.4["event_type"], AUDIT_EVENT_TYPE);
            assert_eq!(row.4["schema_id"], AUDIT_SCHEMA_ID);
            assert_eq!(row.4["schema_version"], AUDIT_SCHEMA_VERSION, "schema_version = AUDIT_SCHEMA_VERSION");
            assert_eq!(
                row.4["occurred_at"].as_str().expect("occurred_at string"),
                expected_occurred,
                "occurred_at = PG jsonb spelling of NEW.created_at (server-stamped, single clock domain)"
            );
            assert_eq!(
                row.4["actor"]["id"], actor.to_uuid().to_string(),
                "actor.id = actor_id::text"
            );
            assert_eq!(
                row.4["actor"]["type"] , AUDIT_ACTOR_TYPE_PARTICIPANT,
                "actor.type = 'participant' (actor_id present)"
            );
            assert_eq!(row.4["targets"], serde_json::json!([]), "target NULL → empty targets");
            assert_eq!(row.4["aggregate_type"], AUDIT_AGGREGATE_TYPE);
            assert_eq!(row.4["aggregate_id"], ws.to_uuid().to_string());
            assert_eq!(
                row.4["action"], *action,
                "outbound action = local token VERBATIM (no fabricated contract token)"
            );
            assert_eq!(row.4["outcome"], AUDIT_OUTCOME_SUCCESS);
            assert_eq!(
                row.4["payload"], serde_json::json!({}),
                "payload = aero_snaplink_audit_payload(detail) — an empty object sanitizes to an empty object"
            );
            assert_eq!(row.4["data_classification"], AUDIT_DATA_CLASSIFICATION);
            assert_eq!(row.4["retention_class"], AUDIT_RETENTION_CLASS);
            assert_eq!(
                row.4["idempotency_key"], id.to_string(),
                "idempotency_key = event_id (sink dedup)"
            );
            // No L1 markers on 1:1 rows (parity SUM side never sees them).
            assert!(row.4.get("count").is_none(), "no count key on 1:1 room rows");
            assert!(row.4.get("aggregated").is_none(), "no aggregated key on 1:1 room rows");
            assert!(row.4.get("spill").is_none(), "no spill key on 1:1 room rows");
        }

        // v1 dual-path: every room audit row ALSO produced a v1 row (0236
        // trigger, action verbatim) — v1/v2 coexistence is by design.
        for (id, action) in &ids {
            let v1: i64 = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM snaplink_delivery_outbox
                  WHERE destination = 'audit' AND idempotency_key = $1",
            )
            .bind(id.to_string())
            .fetch_one(&p)
            .await
            .unwrap()
            .0;
            assert_eq!(v1, 1, "v1 row still produced per room audit row (dual-path)");
            let v1_action: String = sqlx::query_scalar(
                "SELECT payload->>'action' FROM snaplink_delivery_outbox
                  WHERE destination = 'audit' AND idempotency_key = $1",
            )
            .bind(id.to_string())
            .fetch_one(&p)
            .await
            .expect("v1 payload action");
            assert_eq!(v1_action, *action, "v1 keeps the local token verbatim");
        }

        // --- Replay half: same-id duplicate audit INSERT (composite PK
        // (id, created_at) admits it) → ON CONFLICT dedups; still N rows.
        // Runs with enforcement DISABLED: while enabled, the 0236 v1
        // trigger's own plain INSERT dedups fail-loud (snaplink_delivery_
        // outbox PK = 'audit:' || id) — the v2 ON CONFLICT dedup contract is
        // what's pinned here (duplicate_event_id_is_deduped_by_on_conflict
        // precedent).
        restore_enforcement_disabled(&p).await;
        let mut tx = p.begin().await.expect("begin replay tx");
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)",
        )
        .bind(ids[0].0)
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .bind(LOCAL_ACTION_ROOM_CREATE)
        .bind(fixed_ts + time::Duration::seconds(1))
        .execute(&mut *tx)
        .await
        .expect("replayed audit row (same id, new created_at) is admitted by the composite PK");
        tx.commit().await.expect("commit replay tx");
        let after_replay: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(
            after_replay,
            i64::try_from(ids.len()).expect("three-room fixture length fits i64"),
            "replay deduped by ON CONFLICT DO NOTHING"
        );

        // --- Rollback half: aborted tx → zero new rows (no partial state). ---
        let before = governance_rows_for(&p, ws).await;
        let mut tx = p.begin().await.expect("begin rollback tx");
        insert_audit_row_returning_id(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
        tx.rollback().await.expect("rollback room tx");
        assert_eq!(
            governance_rows_for(&p, ws).await,
            before,
            "AFTER-trigger rollback aborts the outbox row with the audit row"
        );
        // Restore the fresh-DB default (see `restore_enforcement_disabled`).
        restore_enforcement_disabled(&p).await;
    }

    /// 0246 presence probe (0242/0245 precedent): on a partially-migrated
    /// shared DB the trigger is absent → explicit SKIP, never a silent green
    /// (the harness `audit_governance::` slot runs on a migrated throwaway
    /// DB, so this only fires on a shared-DB run or a stale `aero-cli`
    /// binary — the AC6 manual-path closure greps for this exact SKIP line
    /// with `--nocapture`).
    async fn recall_trigger_migrated(p: &PgPool) -> bool {
        let probe: Option<String> = sqlx::query_scalar(
            "SELECT to_regprocedure('aero_enqueue_message_recall_audit()')::text",
        )
        .fetch_one(p)
        .await
        .expect("probe for the 0246 function");
        if probe.is_none() {
            eprintln!("SKIP: 0246 not migrated (aero_enqueue_message_recall_audit missing)");
            return false;
        }
        true
    }

    /// B5-1 R2 — message-recall lane outbox parity (migration 0246, harness
    /// slot `audit_governance::`): N `message.recalled` audit appends in one
    /// tx via the REAL producer seam `AuditRepo::append_in_tx` (the
    /// `authorization.rs` recall path — REQUIRED, NOT the
    /// `insert_audit_row_returning_id` helper, whose hardcoded detail `'{}'`
    /// can never produce the recall `{room_id, digest}` payload) → exactly N
    /// 1:1 outbox rows, status 0, class 'message' (leaf const
    /// `GOVERNANCE_CLASS_MESSAGE`), priority 10 comment-pinned
    /// (`GOVERNANCE_PRIORITY_BACKLOG`; aero-storage cannot import aero-ai),
    /// ALL 16 envelope keys field-by-field (recall-specific expectations:
    /// non-empty `targets = [{id: <message id>, type: 'resource'}]`, payload
    /// = the sanitized `{room_id, digest}` detail object, `occurred_at` = the
    /// PG jsonb spelling of `NEW.created_at`, actor = the recaller), action
    /// verbatim `LOCAL_ACTION_MESSAGE_RECALLED`, no L1 marker keys; each
    /// payload parses into `AuditClaimPayload` (`deny_unknown_fields`) and is
    /// Value-equal. Rollback half → 0 rows; replay half (same-id duplicate
    /// with a VARYING `created_at` — a fixed-ts duplicate would hit the
    /// composite PK (`id`, `created_at`) instead of exercising dedup — and
    /// enforcement re-asserted disabled so the 0236 v1 plain INSERT cannot
    /// fail-loud on its own PK) → still exactly N rows.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recall_lane_outbox_parity() {
        let p = pool();
        if !recall_trigger_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        let (ws, actor) = fixture(&p).await;
        // Fresh-DB default (the singleton is GLOBAL; the harness runs the
        // ignored suite on shared DBs) — with enforcement disabled the 0236
        // v1 trigger produces no v1 rows, so this test is v2-lane-only by
        // design (0246 is gate-free: the unconditional-enqueue property is
        // itself asserted, mirroring `room_lane_unconditional_enqueue`).
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");
        let message_ids: Vec<MessageId> = (0..3).map(|_| MessageId::new()).collect();
        let room_id = RoomId::new();
        let digest: String = "r".repeat(120); // first 120 chars, authorization.rs:300 shape
        let detail = serde_json::json!({ "room_id": room_id, "digest": digest });

        // --- Commit half: N message.recalled appends via the real producer
        // seam, one tx (the authorization.rs:368 path: actor = the recaller,
        // target = Some(message id), detail = {room_id, digest}). ---
        let mut tx = p.begin().await.expect("begin recall parity tx");
        let mut ids: Vec<AuditId> = Vec::new();
        for message_id in &message_ids {
            let id = crate::AuditRepo::append_in_tx(
                &mut tx,
                ws,
                Some(actor),
                LOCAL_ACTION_MESSAGE_RECALLED,
                Some(&message_id.to_string()),
                detail.clone(),
            )
            .await
            .expect("append recall audit row");
            ids.push(id);
        }
        tx.commit().await.expect("commit recall parity tx");

        // Exactly N outbox rows, 1:1, all mapped fields pinned.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
                .fetch_one(&p)
                .await
                .expect("count recall rows");
        assert_eq!(
            count,
            i64::try_from(ids.len()).expect("three recall ids fit i64"),
            "N recall appends → exactly N 1:1 outbox rows (no aggregation, no duplicates)"
        );
        for (idx, id) in ids.iter().enumerate() {
            let row: (i32, String, i16, serde_json::Value) = sqlx::query_as(
                "SELECT status, class, priority, payload
                   FROM audit_governance_outbox WHERE event_id = $1",
            )
            .bind(id.to_uuid())
            .fetch_one(&p)
            .await
            .expect("recall outbox row");
            assert_eq!(row.0, 0, "status 0 = enqueued (0239 normative)");
            assert_eq!(
                row.1, GOVERNANCE_CLASS_MESSAGE,
                "class 'message' (GOVERNANCE_CLASS_MESSAGE leaf const)"
            );
            assert_eq!(
                row.2, 10,
                "priority 10 = GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import aero-ai)"
            );
            // occurred_at: server-stamped created_at (append_on now_utc()) →
            // PG jsonb spelling, derived from the stored audit row (never a
            // Rust formatter).
            let created_at: time::OffsetDateTime =
                sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
                    .bind(id.to_uuid())
                    .fetch_one(&p)
                    .await
                    .expect("audit row created_at");
            let expected_occurred: String =
                sqlx::query_scalar("SELECT (to_jsonb($1::timestamptz))::text")
                    .bind(created_at)
                    .fetch_one(&p)
                    .await
                    .expect("PG jsonb spelling of occurred_at");
            let expected_occurred = expected_occurred.trim_matches('"');
            // Envelope field-by-field (recomputed from the leaf consts).
            assert_eq!(row.3["event_id"], id.to_uuid().to_string(), "payload event_id mirrors");
            assert_eq!(
                row.3["source_system"], AUDIT_SOURCE_SYSTEM,
                "payload source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
            );
            assert_eq!(row.3["event_type"], AUDIT_EVENT_TYPE);
            assert_eq!(row.3["schema_id"], AUDIT_SCHEMA_ID);
            assert_eq!(row.3["schema_version"], AUDIT_SCHEMA_VERSION);
            assert_eq!(
                row.3["occurred_at"].as_str().expect("occurred_at string"),
                expected_occurred,
                "occurred_at = PG jsonb spelling of NEW.created_at (server-stamped, single clock domain)"
            );
            assert_eq!(
                row.3["actor"]["id"], actor.to_uuid().to_string(),
                "actor.id = the RECALLER's participant id (recall authority, not the message author)"
            );
            assert_eq!(
                row.3["actor"]["type"], AUDIT_ACTOR_TYPE_PARTICIPANT,
                "actor.type = 'participant' (actor_id present on the recall producer)"
            );
            assert_eq!(
                row.3["targets"],
                serde_json::json!([{ "id": message_ids[idx].to_string(), "type": "resource" }]),
                "targets = [{{id: <message id>, type: 'resource'}}] — non-empty (recall target is the message)"
            );
            assert_eq!(row.3["aggregate_type"], AUDIT_AGGREGATE_TYPE);
            assert_eq!(row.3["aggregate_id"], ws.to_uuid().to_string());
            assert_eq!(
                row.3["action"], LOCAL_ACTION_MESSAGE_RECALLED,
                "action = local token VERBATIM (no fabricated contract token)"
            );
            assert_eq!(row.3["outcome"], AUDIT_OUTCOME_SUCCESS);
            assert_eq!(
                row.3["payload"], detail,
                "payload = aero_snaplink_audit_payload(detail) — the {{room_id, digest}} object sanitizes unchanged"
            );
            assert_eq!(row.3["data_classification"], AUDIT_DATA_CLASSIFICATION);
            assert_eq!(row.3["retention_class"], AUDIT_RETENTION_CLASS);
            assert_eq!(
                row.3["idempotency_key"], id.to_uuid().to_string(),
                "idempotency_key = event_id (sink dedup)"
            );
            // No L1 markers on 1:1 rows (parity SUM side never sees them).
            assert!(row.3.get("count").is_none(), "no count key on 1:1 recall rows");
            assert!(row.3.get("aggregated").is_none(), "no aggregated key on 1:1 recall rows");
            assert!(row.3.get("spill").is_none(), "no spill key on 1:1 recall rows");
            assert!(row.3.get("window_start").is_none(), "no window_start key on 1:1 recall rows");
            assert!(row.3.get("window_end").is_none(), "no window_end key on 1:1 recall rows");
            // Fail-closed parse into the typed twin + Value equality.
            let parsed: AuditClaimPayload = serde_json::from_value(row.3.clone())
                .expect("typed twin parses the 0246 payload (deny_unknown_fields = drift alarm)");
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                row.3,
                "re-serialized twin must equal the stored JSONB (semantic parity)"
            );
            assert_eq!(
                parsed.action, LOCAL_ACTION_MESSAGE_RECALLED,
                "typed action == the leaf const verbatim"
            );
        }

        // --- Replay half: same-id duplicate audit INSERT (the composite PK
        // (id, created_at) admits it only with a VARYING created_at) → the
        // 0246 trigger re-fires → ON CONFLICT dedups → still N rows. Runs
        // with enforcement DISABLED (re-asserted): while enabled, the 0236 v1
        // trigger's own plain INSERT dedups fail-loud (snaplink_delivery_
        // outbox PK = 'audit:' || id) — the v2 ON CONFLICT dedup contract is
        // what's pinned here (room_lane_outbox_parity replay precedent).
        restore_enforcement_disabled(&p).await;
        let mut tx = p.begin().await.expect("begin replay tx");
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ids[0].to_uuid())
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_RECALLED)
        .bind(message_ids[0].to_string())
        .bind(sqlx::types::Json(detail.clone()))
        .bind(fixed_ts + time::Duration::seconds(60))
        .execute(&mut *tx)
        .await
        .expect("replayed audit row (same id, varied created_at) is admitted by the composite PK");
        tx.commit().await.expect("commit replay tx");
        let after_replay: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
                .fetch_one(&p)
                .await
                .expect("count after replay");
        assert_eq!(
            after_replay,
            i64::try_from(ids.len()).expect("three recall ids fit i64"),
            "replay deduped by ON CONFLICT DO NOTHING"
        );

        // --- Rollback half: aborted tx → zero new rows (no partial state). ---
        let before = count;
        let mut tx = p.begin().await.expect("begin rollback tx");
        crate::AuditRepo::append_in_tx(
            &mut tx,
            ws,
            Some(actor),
            LOCAL_ACTION_MESSAGE_RECALLED,
            Some(&message_ids[0].to_string()),
            detail.clone(),
        )
        .await
        .expect("append rollback recall row");
        tx.rollback().await.expect("rollback recall tx");
        let after_rollback: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
                .fetch_one(&p)
                .await
                .expect("count after rollback");
        assert_eq!(
            after_rollback, before,
            "AFTER-trigger rollback aborts the outbox row with the audit row"
        );
    }

    /// A1.2 — message-lane outbox parity (harness slot
    /// `message_lane_outbox_parity`): the L1 aggregated lane's COUNT/SUM
    /// balance re-grounded as a `db_test` — SUM(count) over window rows + spill
    /// rows == COUNT(mapped message.create/edit audit rows), scoped to the
    /// retention cutoff on BOTH sides (the drill `parity` query mirror,
    /// aero-audit-l1-parity-drill.rs :296-325 — the window-start grid is
    /// floored the same way, so a late event in the current minute balances).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_lane_outbox_parity() {
        let p = pool();
        if !l1_aggregate_migrated(&p).await || !room_trigger_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        // Fresh-DB default (the singleton is global; harness runs the ignored
        // suite on shared DBs).
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");
        let cutoff_epoch: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM $1::timestamptz) / 60)::bigint * 60",
        )
        .bind(fixed_ts)
        .fetch_one(&p)
        .await
        .expect("retention cutoff (minute floor, both sides)");

        // 6 mapped audit rows (5 create + 1 edit) in one tx, same window.
        let mut tx = p.begin().await.expect("begin message parity tx");
        for _ in 0..5 {
            insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        }
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_EDIT, fixed_ts).await;
        // Plus unmapped noise that must NOT balance anything: room rows are
        // 1:1 (their own lane) and message.deleted stays unmapped (R-D2).
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
        insert_audit_row(&mut tx, ws, actor, "message.deleted", fixed_ts).await;
        tx.commit().await.expect("commit message parity tx");

        // SUM side (drill mirror): window + spill rows, class 'message'.
        let sum: i64 = sqlx::query_scalar(
            r"SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
                FROM audit_governance_outbox
               WHERE class = 'message'
                 AND ((payload->>'aggregated') = 'true' OR (payload->>'spill') = 'true')
                 AND floor(extract(epoch FROM (payload->>'window_start')::timestamptz) / 60) * 60
                     >= $2",
        )
        .bind(ws.to_uuid().to_string())
        .bind(cutoff_epoch)
        .fetch_one(&p)
        .await
        .expect("parity SUM");
        // COUNT side (drill mirror): allowlisted audit rows, same grid.
        let count: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*)::bigint
                FROM audit_events
               WHERE workspace_id = $1
                 AND action IN ($2, $3)
                 AND floor(extract(epoch FROM created_at) / 60) * 60 >= $4",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_CREATE)
        .bind(LOCAL_ACTION_MESSAGE_EDIT)
        .bind(cutoff_epoch)
        .fetch_one(&p)
        .await
        .expect("parity COUNT");
        assert_eq!(
            sum, count,
            "SUM(window counts) + spill == COUNT(mapped message.create/edit audit rows)"
        );
        assert_eq!(sum, 6, "sanity: 5 create + 1 edit balance exactly");
        // The room row is NOT counted on either side (its own 1:1 lane), and
        // message.deleted stays unmapped (R-D2) — both invisible to the
        // message-lane parity by construction.
        let room_rows: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE class = 'room' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(room_rows, 1, "room.create produced exactly one 1:1 room row");
    }

    /// A5.2 — room rows never merge into (or spill from) the L1 message
    /// window: 2 room.create rows with IDENTICAL `created_at` (same window) →
    /// 2 distinct 1:1 rows (`event_id` = each audit id), no count/aggregated/
    /// spill keys; a room row committed while the message window row is
    /// claimed (status 2) → NO spill row (the 0242 allowlist gate returns
    /// NEW before any window/spill work for room tokens).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn room_lane_never_merged_into_l1_window() {
        let p = pool();
        if !l1_aggregate_migrated(&p).await || !room_trigger_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");

        // 2 room.create rows, identical created_at → 2 distinct 1:1 rows.
        let mut tx = p.begin().await.expect("begin room-window tx");
        let mut room_ids = Vec::new();
        for _ in 0..2 {
            room_ids.push(
                insert_audit_row_returning_id(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts)
                    .await,
            );
        }
        tx.commit().await.expect("commit room-window tx");
        let room_rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT event_id::text, payload FROM audit_governance_outbox
              WHERE class = 'room' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read room rows");
        assert_eq!(room_rows.len(), 2, "same-window room rows must NOT merge");
        let mut seen = std::collections::HashSet::new();
        for (event_id, payload) in &room_rows {
            assert!(seen.insert(event_id.clone()), "event_id must be distinct per room audit row");
            assert!(room_ids.iter().any(|id| &id.to_string() == event_id));
            assert!(payload.get("count").is_none(), "no count key on room rows");
            assert!(payload.get("aggregated").is_none(), "no aggregated key on room rows");
            assert!(payload.get("spill").is_none(), "no spill key on room rows");
        }

        // Message window row, then flip it claimed (status 2) — a late room
        // row in the SAME window must neither merge nor spill.
        let mut tx = p.begin().await.expect("begin claimed-window tx");
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_CREATE, fixed_ts).await;
        tx.commit().await.expect("commit claimed-window tx");
        let window_key = recompute_window_key(&p, ws, fixed_ts).await;
        sqlx::query("UPDATE audit_governance_outbox SET status = 2 WHERE event_id = $1")
            .bind(window_key)
            .execute(&p)
            .await
            .expect("flip window row to status 2 (delivered)");
        let mut tx = p.begin().await.expect("begin late-room tx");
        let late_id =
            insert_audit_row_returning_id(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts)
                .await;
        tx.commit().await.expect("commit late-room tx");
        let spill_rows: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1 AND (payload->>'spill') = 'true'",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(
            spill_rows, 0,
            "a room row must never spill, even with the message window claimed"
        );
        let room_after: Vec<(String,)> = sqlx::query_as(
            "SELECT event_id::text FROM audit_governance_outbox
              WHERE class = 'room' AND payload->>'aggregate_id' = $1",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read room rows after late insert");
        assert_eq!(room_after.len(), 3, "late room row = a third 1:1 row, never a spill");
        assert!(
            room_after.iter().any(|(id,)| id == &late_id.to_string()),
            "late room row is its own 1:1 row (event_id = audit id)"
        );
    }

    /// A2.4 — room lane enqueues UNCONDITIONALLY (0242 D2 precedent): with
    /// commercial enforcement DISABLED and NO binding row, room.* audit rows
    /// still produce their outbox rows (contrast: the 0239 moderation lane
    /// Gate 1 pass-through produces nothing while disabled). Non-allowlisted
    /// tokens — `room.creat` (prefix typo), `auth.login`, `message.deleted`
    /// (R-D2) — produce ZERO rows. NO v1 assertion here (F-A split: with
    /// enforcement disabled the 0236 v1 trigger returns NEW before producing
    /// a v1 row).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn room_lane_unconditional_enqueue() {
        let p = pool();
        if !room_trigger_migrated(&p).await {
            return;
        }
        reset_governance_table(&p).await;
        sqlx::query(
            "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
        )
        .execute(&p)
        .await
        .expect("re-assert enforcement disabled");
        let (ws, actor) = fixture(&p).await;
        let fixed_ts: time::OffsetDateTime =
            sqlx::query_scalar("SELECT now()").fetch_one(&p).await.expect("capture fixed ts");

        // Allowlisted tokens enqueue with zero gate prerequisites.
        let mut tx = p.begin().await.expect("begin unconditional tx");
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
        insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_ARCHIVED, fixed_ts).await;
        // Non-allowlisted tokens must pass through unmapped.
        insert_audit_row(&mut tx, ws, actor, "room.creat", fixed_ts).await;
        insert_audit_row(&mut tx, ws, actor, "auth.login", fixed_ts).await;
        insert_audit_row(&mut tx, ws, actor, "message.deleted", fixed_ts).await;
        tx.commit().await.expect("commit unconditional tx");

        let rows: Vec<(String, i16)> = sqlx::query_as(
            "SELECT class, priority FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1 ORDER BY class",
        )
        .bind(ws.to_uuid().to_string())
        .fetch_all(&p)
        .await
        .expect("read rows");
        assert_eq!(
            rows.len(),
            2,
            "exactly the two allowlisted room tokens enqueue (runtime disabled, no binding)"
        );
        assert!(
            rows.iter().all(|(class, priority)| {
                class == GOVERNANCE_CLASS_ROOM && *priority == 10
            }),
            "all rows are class 'room', priority 10"
        );
        let v1_for_room_create: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM snaplink_delivery_outbox s
              JOIN audit_events a ON a.id::text = s.idempotency_key
             WHERE a.workspace_id = $1 AND a.action = $2",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_ROOM_CREATE)
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(
            v1_for_room_create, 0,
            "no v1 row while enforcement disabled (the dual-path assertion lives in room_lane_outbox_parity)"
        );
    }
}
