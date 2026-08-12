use super::*;

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
    /// the `EvalPlanQual` path directly): two parallel txs insert one
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

