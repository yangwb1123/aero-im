use super::*;

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

