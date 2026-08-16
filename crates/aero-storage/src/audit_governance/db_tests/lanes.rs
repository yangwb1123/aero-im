use super::*;

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
    let fixed_ts: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&p)
        .await
        .expect("capture fixed ts");
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
    let expected_occurred: String = sqlx::query_scalar("SELECT (to_jsonb($1::timestamptz))::text")
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
        assert_eq!(
            row.4["event_id"],
            id.to_string(),
            "payload event_id mirrors"
        );
        assert_eq!(
            row.4["source_system"], AUDIT_SOURCE_SYSTEM,
            "payload source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
        );
        assert_eq!(row.4["event_type"], AUDIT_EVENT_TYPE);
        assert_eq!(row.4["schema_id"], AUDIT_SCHEMA_ID);
        assert_eq!(
            row.4["schema_version"], AUDIT_SCHEMA_VERSION,
            "schema_version = AUDIT_SCHEMA_VERSION"
        );
        assert_eq!(
                row.4["occurred_at"].as_str().expect("occurred_at string"),
                expected_occurred,
                "occurred_at = PG jsonb spelling of NEW.created_at (server-stamped, single clock domain)"
            );
        assert_eq!(
            row.4["actor"]["id"],
            actor.to_uuid().to_string(),
            "actor.id = actor_id::text"
        );
        assert_eq!(
            row.4["actor"]["type"], AUDIT_ACTOR_TYPE_PARTICIPANT,
            "actor.type = 'participant' (actor_id present)"
        );
        assert_eq!(
            row.4["targets"],
            serde_json::json!([]),
            "target NULL → empty targets"
        );
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
            row.4["idempotency_key"],
            id.to_string(),
            "idempotency_key = event_id (sink dedup)"
        );
        // No L1 markers on 1:1 rows (parity SUM side never sees them).
        assert!(
            row.4.get("count").is_none(),
            "no count key on 1:1 room rows"
        );
        assert!(
            row.4.get("aggregated").is_none(),
            "no aggregated key on 1:1 room rows"
        );
        assert!(
            row.4.get("spill").is_none(),
            "no spill key on 1:1 room rows"
        );
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
        assert_eq!(
            v1, 1,
            "v1 row still produced per room audit row (dual-path)"
        );
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
    let fixed_ts: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&p)
        .await
        .expect("capture fixed ts");
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
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
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
        assert_eq!(
            row.3["event_id"],
            id.to_uuid().to_string(),
            "payload event_id mirrors"
        );
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
            row.3["actor"]["id"],
            actor.to_uuid().to_string(),
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
            row.3["idempotency_key"],
            id.to_uuid().to_string(),
            "idempotency_key = event_id (sink dedup)"
        );
        // No L1 markers on 1:1 rows (parity SUM side never sees them).
        assert!(
            row.3.get("count").is_none(),
            "no count key on 1:1 recall rows"
        );
        assert!(
            row.3.get("aggregated").is_none(),
            "no aggregated key on 1:1 recall rows"
        );
        assert!(
            row.3.get("spill").is_none(),
            "no spill key on 1:1 recall rows"
        );
        assert!(
            row.3.get("window_start").is_none(),
            "no window_start key on 1:1 recall rows"
        );
        assert!(
            row.3.get("window_end").is_none(),
            "no window_end key on 1:1 recall rows"
        );
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
    let fixed_ts: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&p)
        .await
        .expect("capture fixed ts");
    let cutoff_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM $1::timestamptz) / 60)::bigint * 60")
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
    // 1:1 (their own lane) and message.deleted stays trigger-unmapped (R-D2; Rust-writer-owned
    // via the delete seam — a raw audit INSERT never fires the writer).
    insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
    insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_DELETED, fixed_ts).await;
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
    // message.deleted stays trigger-unmapped (R-D2; Rust-writer-owned via the
    // delete seam — a raw audit INSERT never fires the writer) — both invisible
    // to the message-lane parity by construction.
    let room_rows: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_governance_outbox
              WHERE class = 'room' AND payload->>'aggregate_id' = $1",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(
        room_rows, 1,
        "room.create produced exactly one 1:1 room row"
    );
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
    let fixed_ts: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&p)
        .await
        .expect("capture fixed ts");

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
        assert!(
            seen.insert(event_id.clone()),
            "event_id must be distinct per room audit row"
        );
        assert!(room_ids.iter().any(|id| &id.to_string() == event_id));
        assert!(payload.get("count").is_none(), "no count key on room rows");
        assert!(
            payload.get("aggregated").is_none(),
            "no aggregated key on room rows"
        );
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
        insert_audit_row_returning_id(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
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
    assert_eq!(
        room_after.len(),
        3,
        "late room row = a third 1:1 row, never a spill"
    );
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
/// (R-D2; Rust-writer-owned via the delete seam — a raw audit INSERT never
/// fires the writer) — produce ZERO rows. NO v1 assertion here (F-A split: with
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
    let fixed_ts: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&p)
        .await
        .expect("capture fixed ts");

    // Allowlisted tokens enqueue with zero gate prerequisites.
    let mut tx = p.begin().await.expect("begin unconditional tx");
    insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_CREATE, fixed_ts).await;
    insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_ROOM_ARCHIVED, fixed_ts).await;
    // Non-allowlisted tokens must pass through unmapped.
    insert_audit_row(&mut tx, ws, actor, "room.creat", fixed_ts).await;
    insert_audit_row(&mut tx, ws, actor, "auth.login", fixed_ts).await;
    insert_audit_row(&mut tx, ws, actor, LOCAL_ACTION_MESSAGE_DELETED, fixed_ts).await;
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
        rows.iter()
            .all(|(class, priority)| { class == GOVERNANCE_CLASS_ROOM && *priority == 10 }),
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

/// R-D2 AC-3 — message-delete lane outbox parity via the REAL producer seam
/// (`soft_delete_outboxed_authorized` — the exact `ImService::delete_message`
/// path): N deletes → exactly N 1:1 class-'message' priority-10 rows written
/// in the delete txs by the Rust writer, ALL 16 envelope keys field-by-field
/// vs the 0239 spelling (`occurred_at` byte-asserted against BOTH the PG
/// `to_jsonb(created_at)` text and the shared `audit_wire_occurred_at`
/// helper — §10.4 canonical spelling; actor = the deleter; non-empty
/// targets; payload = the `{room_id, digest}` detail; action verbatim
/// `LOCAL_ACTION_MESSAGE_DELETED`; `idempotency_key == event_id`; no L1
/// marker keys), each payload parses into `AuditClaimPayload`
/// (`deny_unknown_fields`) and re-serializes Value-equal. Rollback half → 0
/// new rows; replay half (same-id audit re-INSERT with a VARYING `created_at`
/// — the composite PK `(id, created_at)` admits it — never fires the writer,
/// which lives only in the delete path) → still exactly N rows.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delete_lane_outbox_parity() {
    let p = pool();
    reset_governance_table(&p).await;
    // Bare fixture (no authorized room-create — that path would self-produce
    // a room.create audit row + 0245 outbox row): workspace + owner actor +
    // one channel room (creator = owner member edge).
    let (ws, actor) = fixture(&p).await;
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            aero_common::RoomKind::Channel,
            Some(format!("delete-lane-parity-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create channel room")
        .id;
    let repo = crate::message::MessageRepo::new(p.clone());

    // --- Commit half: N messages + N authorized deletes (each delete commits
    // its audit row + writer outbox row atomically). Bare `insert` (no audit
    // append — no window-row pollution). ---
    let mut message_ids = Vec::new();
    for i in 0..3 {
        let msg = repo
            .insert(crate::message::NewMessage {
                room_id: room,
                sender_id: actor,
                blocks: vec![aero_common::Block::text(format!("delete-lane-{i}"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("seed message");
        repo.soft_delete_outboxed_authorized(msg.id, actor, None)
            .await
            .expect("authorized delete commits")
            .expect("message deleted");
        message_ids.push(msg);
    }

    let audit_rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, target FROM audit_events
          WHERE workspace_id = $1 AND action = $2
          ORDER BY created_at, id",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_DELETED)
    .fetch_all(&p)
    .await
    .expect("query delete audit rows");
    assert_eq!(
        audit_rows.len(),
        3,
        "N deletes → N message.deleted audit rows"
    );
    let delete_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("delete outbox count");
    assert_eq!(
        delete_rows, 3,
        "N deletes → exactly N 1:1 message-class outbox rows (no aggregation, no duplicates)"
    );

    // Field-by-field envelope pins (G2 closure — recomputed from leaf consts
    // and the PG jsonb spelling, never inline literals).
    for ((audit_id, target), msg) in audit_rows.iter().zip(&message_ids) {
        assert_eq!(*target, msg.id.to_string(), "target = message id");
        let row: (i32, String, i16, serde_json::Value) = sqlx::query_as(
            "SELECT status, class, priority, payload
                   FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(*audit_id)
        .fetch_one(&p)
        .await
        .expect("delete outbox row");
        assert_eq!(row.0, 0, "status 0 = enqueued (0239 normative)");
        assert_eq!(
            row.1, GOVERNANCE_CLASS_MESSAGE,
            "class 'message' (GOVERNANCE_CLASS_MESSAGE leaf const)"
        );
        assert_eq!(
            row.2, 10,
            "priority 10 = GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import aero-ai)"
        );
        let envelope = row.3;
        // occurred_at byte-parity: the envelope spelling must equal BOTH the
        // PG `to_jsonb(created_at)` text (the trigger's ground truth) and the
        // shared helper (single canonical spelling, §10.4).
        let created_at: time::OffsetDateTime =
            sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
                .bind(*audit_id)
                .fetch_one(&p)
                .await
                .expect("audit row created_at");
        let pg_spelling: String =
            sqlx::query_scalar("SELECT (to_jsonb($1::timestamptz))::text")
                .bind(created_at)
                .fetch_one(&p)
                .await
                .expect("PG jsonb spelling");
        let pg_spelling = pg_spelling.trim_matches('"').to_owned();
        let helper_spelling = aero_common::audit_wire_occurred_at(created_at);
        assert_eq!(pg_spelling, helper_spelling, "helper == PG spelling");
        assert_eq!(
            envelope["occurred_at"].as_str().expect("occurred_at string"),
            pg_spelling,
            "R-D2 envelope occurred_at == the canonical PG spelling (byte-identical)"
        );
        assert_eq!(
            envelope["event_id"],
            audit_id.to_string(),
            "payload event_id mirrors"
        );
        assert_eq!(
            envelope["source_system"], AUDIT_SOURCE_SYSTEM,
            "payload source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
        );
        assert_eq!(envelope["event_type"], AUDIT_EVENT_TYPE);
        assert_eq!(envelope["schema_id"], AUDIT_SCHEMA_ID);
        assert_eq!(envelope["schema_version"], AUDIT_SCHEMA_VERSION);
        assert_eq!(
            envelope["actor"]["id"],
            actor.to_uuid().to_string(),
            "actor.id = the DELETER's participant id"
        );
        assert_eq!(
            envelope["actor"]["type"], AUDIT_ACTOR_TYPE_PARTICIPANT,
            "actor.type = 'participant' (deleter is a human actor)"
        );
        assert_eq!(
                envelope["targets"],
                serde_json::json!([{ "id": msg.id.to_string(), "type": "resource" }]),
                "targets = [{{id: <message id>, type: 'resource'}}] — non-empty (delete target is the message)"
            );
        assert_eq!(envelope["aggregate_type"], AUDIT_AGGREGATE_TYPE);
        assert_eq!(envelope["aggregate_id"], ws.to_uuid().to_string());
        assert_eq!(
            envelope["action"], LOCAL_ACTION_MESSAGE_DELETED,
            "action = local token VERBATIM (no fabricated contract token)"
        );
        assert_eq!(envelope["outcome"], AUDIT_OUTCOME_SUCCESS);
        assert_eq!(
                envelope["payload"],
                serde_json::json!({
                    // RoomId serializes in its OWN Display format (the same
                    // serde the producer uses) — never to_uuid().to_string()
                    // (uuid::text differs, e.g. hyphens).
                    "room_id": room,
                    "digest": msg.searchable_text().chars().take(120).collect::<String>(),
                }),
                "payload = the {{room_id, digest}} detail object (authorization.rs shape)"
            );
        assert_eq!(envelope["data_classification"], AUDIT_DATA_CLASSIFICATION);
        assert_eq!(envelope["retention_class"], AUDIT_RETENTION_CLASS);
        assert_eq!(
            envelope["idempotency_key"],
            audit_id.to_string(),
            "idempotency_key = event_id (sink dedup)"
        );
        for marker in ["count", "aggregated", "spill", "window_start", "window_end"] {
            assert!(
                envelope.get(marker).is_none(),
                "no L1 marker key {marker} on 1:1 delete rows"
            );
        }
        // Fail-closed typed parse + Value-level re-serialize equality.
        let parsed: AuditClaimPayload = serde_json::from_value(envelope.clone())
            .expect("typed twin parses the R-D2 payload (deny_unknown_fields = drift alarm)");
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            envelope,
            "re-serialized twin must equal the stored JSONB (semantic parity)"
        );
        assert_eq!(
            parsed.action, LOCAL_ACTION_MESSAGE_DELETED,
            "typed action == the leaf const verbatim"
        );
    }

    // --- Replay half: re-delete is Ok(None) (deleted_at guard — the writer
    // never re-fires); a raw same-id audit re-INSERT (varying created_at) is
    // admitted by the composite PK but never fires the writer (it lives only
    // in the delete path) → still exactly N rows (at-most-one). ---
    let replay = repo
        .soft_delete_outboxed_authorized(message_ids[0].id, actor, None)
        .await
        .expect("replay delete returns Ok");
    assert!(replay.is_none(), "replay of an already-deleted message is Ok(None)");
    let mut tx = p.begin().await.expect("begin replay tx");
    sqlx::query(
        "INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(audit_rows[0].0)
    .bind(ws.to_uuid())
    .bind(actor.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_DELETED)
    .bind(message_ids[0].id.to_string())
    .bind(sqlx::types::Json(serde_json::json!({ "room_id": room })))
    .bind(
        sqlx::query_scalar::<_, time::OffsetDateTime>("SELECT now()")
            .fetch_one(&p)
            .await
            .expect("replay ts"),
    )
    .execute(&mut *tx)
    .await
    .expect("replayed audit row (same id, varied created_at) is admitted by the composite PK");
    tx.commit().await.expect("commit replay tx");
    let after_replay: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("count after replay");
    assert_eq!(
        after_replay, 3,
        "replay produces no second row (at-most-one — writer fires only on the delete path)"
    );

    // --- Rollback half: aborted tx → zero new rows (no partial state). ---
    let rollback_msg = repo
        .insert(crate::message::NewMessage {
            room_id: room,
            sender_id: actor,
            blocks: vec![aero_common::Block::text("delete-lane-rollback")],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("seed rollback message");
    let mut tx = p.begin().await.expect("begin rollback tx");
    let existing = crate::message::MessageRepo::lock_message_in_tx(&mut tx, rollback_msg.id)
        .await
        .expect("lock the message")
        .expect("message row exists");
    crate::message::MessageRepo::soft_delete_locked_outboxed_in_tx(
        &mut tx,
        existing,
        Some(ws),
        Some(actor),
        Some(LOCAL_ACTION_MESSAGE_DELETED),
        serde_json::json!({ "room_id": room }),
        actor,
        None,
    )
    .await
    .expect("in-tx delete succeeds")
    .expect("deleted");
    tx.rollback().await.expect("rollback delete tx");
    let after_rollback: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("count after rollback");
    assert_eq!(
        after_rollback, 3,
        "rollback aborts the writer's outbox row with the delete (in-tx atomicity)"
    );
}
