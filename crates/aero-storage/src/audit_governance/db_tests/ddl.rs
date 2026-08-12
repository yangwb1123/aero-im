use super::*;

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
