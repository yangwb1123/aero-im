//! Integration-style unit tests for the pure `audit_provision` layer.
//!
//! Kept out of the lib file to stay under the 800-line file-size WARN line.

use aero_eng::audit_provision::*;

fn snapshot(v1: V1OutboxCounts, g0239: Option<G0239Counts>) -> AuditSnapshot {
    AuditSnapshot {
        relay_enabled: false,
        enabled_bindings: 0,
        v1,
        g0239,
        priority_landed: false,
        class_landed: false,
    }
}

fn empty_v1() -> V1OutboxCounts {
    V1OutboxCounts {
        pending: 0,
        claimed: 0,
        delivered: 0,
    }
}

// -- verdict matrix (A1/A3) --

#[test]
fn verdict_relay_off_with_undelivered_is_fail_closed() {
    let s = snapshot(
        V1OutboxCounts {
            pending: 1,
            claimed: 0,
            delivered: 0,
        },
        None,
    );
    let v = verdict(&s);
    assert!(matches!(v, Verdict::FailClosed(_)));
    let Verdict::FailClosed(reason) = v else {
        unreachable!();
    };
    assert!(reason.contains("no audit:event:write grant issued"));
    assert!(reason.contains("relay disabled"));
}

#[test]
fn verdict_relay_off_claimed_rows_also_fail_closed() {
    let s = snapshot(
        V1OutboxCounts {
            pending: 0,
            claimed: 2,
            delivered: 5,
        },
        None,
    );
    assert!(matches!(verdict(&s), Verdict::FailClosed(_)));
}

#[test]
fn verdict_relay_off_empty_is_consistent() {
    let s = snapshot(empty_v1(), None);
    assert_eq!(verdict(&s), Verdict::Consistent);
}

#[test]
fn verdict_relay_on_bindings_zero_is_fail_closed_when_undelivered() {
    // enabled=true but zero bindings ⇒ no grant possible (fail-closed
    // preserved when the bindings table is empty).
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 0,
        v1: V1OutboxCounts {
            pending: 1,
            claimed: 0,
            delivered: 0,
        },
        g0239: None,
        priority_landed: false,
        class_landed: false,
    };
    assert!(matches!(verdict(&s), Verdict::FailClosed(_)));
}

#[test]
fn verdict_relay_on_with_backlog_is_healthy() {
    // Pending backlog under a healthy relay is normal — exit 0.
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 2,
        v1: V1OutboxCounts {
            pending: 7,
            claimed: 1,
            delivered: 10,
        },
        g0239: Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 3,
            claimed: 0,
            delivered: 0,
            dead: 0,
            oldest_pending_secs: Some(0),
            dead_rows: Vec::new(),
        }),
        priority_landed: true,
        class_landed: true,
    };
    assert_eq!(verdict(&s), Verdict::Healthy);
}

#[test]
fn verdict_dead_is_fail_closed_even_with_relay_on() {
    // A3: a dead row is terminal and fails the gate even under a healthy
    // relay — dead is never counted delivered.
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 1,
        v1: empty_v1(),
        g0239: Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 0,
            claimed: 0,
            delivered: 0,
            dead: 1,
            oldest_pending_secs: Some(42),
            dead_rows: vec![(
                "00000000-0000-0000-0000-000000000001".to_string(),
                "403 provisioning refusal".to_string(),
            )],
        }),
        priority_landed: true,
        class_landed: true,
    };
    let v = verdict(&s);
    assert!(matches!(v, Verdict::FailClosed(_)));
    let Verdict::FailClosed(reason) = v else {
        unreachable!();
    };
    assert!(reason.contains("dead"), "reason: {reason}");
    assert!(reason.contains("never counted delivered"));
}

#[test]
fn verdict_dead_priority_over_relay_disabled() {
    // dead > 0 takes priority even when the relay is also off.
    let s = snapshot(
        V1OutboxCounts {
            pending: 3,
            claimed: 0,
            delivered: 0,
        },
        Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 0,
            claimed: 0,
            delivered: 0,
            dead: 2,
            oldest_pending_secs: None,
            dead_rows: Vec::new(),
        }),
    );
    let v = verdict(&s);
    let Verdict::FailClosed(reason) = v else {
        panic!("expected FailClosed, got {v:?}");
    };
    assert!(reason.contains("2 dead row(s)"));
    assert!(!reason.contains("relay disabled"));
}

#[test]
fn verdict_0239_undelivered_counts_with_relay_off() {
    // 0239 pending/claimed rows also count as undelivered (A1).
    let s = snapshot(
        empty_v1(),
        Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 2,
            claimed: 1,
            delivered: 0,
            dead: 0,
            oldest_pending_secs: Some(0),
            dead_rows: Vec::new(),
        }),
    );
    let v = verdict(&s);
    let Verdict::FailClosed(reason) = v else {
        panic!("expected FailClosed, got {v:?}");
    };
    assert!(reason.contains("3 undelivered audit row(s)"));
}

// -- report format (A2) --

#[test]
fn report_contains_greppable_lines_with_four_buckets_and_age() {
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 2,
        v1: V1OutboxCounts {
            pending: 0,
            claimed: 0,
            delivered: 0,
        },
        g0239: Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 2,
            claimed: 0,
            delivered: 10,
            dead: 0,
            oldest_pending_secs: Some(0),
            dead_rows: Vec::new(),
        }),
        priority_landed: true,
        class_landed: true,
    };
    let report = format_report(&s, &verdict(&s));
    assert!(report.contains("audit-provision-check: relay: enabled=true bindings=2"));
    assert!(report.contains("audit-provision-check: v1-outbox: pending=0 claimed=0 delivered=0"));
    assert!(report.contains(
            "audit-provision-check: outbox-0239: table=audit_governance_outbox pending=2 claimed=0 delivered=10 dead=0"
        ));
    assert!(report.contains("audit-provision-check: oldest-pending-age: 0s"));
    assert!(report.contains("audit-provision-check: verdict: healthy — relay enabled (2 bindings)"));
}

#[test]
fn report_not_migrated_has_no_age_or_dead_lines() {
    let s = snapshot(empty_v1(), None);
    let report = format_report(&s, &verdict(&s));
    assert!(report.contains("audit-provision-check: outbox-0239: not migrated"));
    assert!(report.contains("audit-provision-check: verdict: consistent"));
    assert!(!report.contains("oldest-pending-age"));
    assert!(!report.contains("audit-provision-check: dead:"));
}

#[test]
fn report_lists_dead_rows_separately_from_delivered() {
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 1,
        v1: empty_v1(),
        g0239: Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 0,
            claimed: 0,
            delivered: 0,
            dead: 1,
            oldest_pending_secs: Some(12),
            dead_rows: vec![(
                "11111111-1111-1111-1111-111111111111".to_string(),
                "403 provisioning refusal (drill)".to_string(),
            )],
        }),
        priority_landed: false,
        class_landed: false,
    };
    let report = format_report(&s, &verdict(&s));
    assert!(report.contains("delivered=0"));
    assert!(report.contains("dead=1"));
    assert!(report.contains(
            "audit-provision-check: dead: 11111111-1111-1111-1111-111111111111 403 provisioning refusal (drill)"
        ));
    assert!(report.contains("verdict: fail-closed"));
}

// -- parsers --

#[test]
fn parse_priority_probe_two_cells() {
    assert_eq!(parse_priority_probe("t|t"), Ok((true, true)));
    assert_eq!(parse_priority_probe("f|t"), Ok((false, true)));
    assert_eq!(parse_priority_probe("t|f"), Ok((true, false)));
    assert_eq!(parse_priority_probe("f|f"), Ok((false, false)));
    assert_eq!(parse_priority_probe(" t|f\n"), Ok((true, false)));
}

#[test]
fn parse_priority_probe_rejects_malformed_lines() {
    assert!(parse_priority_probe("t").is_err());
    assert!(parse_priority_probe("x|t").is_err());
    assert!(parse_priority_probe("").is_err());
    assert!(parse_priority_probe("t|t|t").is_err());
}

// -- B5-3 priority/class verdict lines --

#[test]
fn report_priority_and_class_verdict_lines() {
    // landed
    let s = AuditSnapshot {
        relay_enabled: true,
        enabled_bindings: 2,
        v1: V1OutboxCounts {
            pending: 0,
            claimed: 0,
            delivered: 0,
        },
        g0239: Some(G0239Counts {
            table: Some("audit_governance_outbox"),
            pending: 0,
            claimed: 0,
            delivered: 0,
            dead: 0,
            oldest_pending_secs: None,
            dead_rows: Vec::new(),
        }),
        priority_landed: true,
        class_landed: true,
    };
    let report = format_report(&s, &verdict(&s));
    assert!(report.contains("audit-provision-check: priority: landed"));
    assert!(report.contains("audit-provision-check: class: landed"));
    // absent (0239 not migrated)
    let absent = snapshot(empty_v1(), None);
    let report = format_report(&absent, &verdict(&absent));
    assert!(report.contains("audit-provision-check: priority: absent"));
    assert!(report.contains("audit-provision-check: class: absent"));
}

// -- parsers --

#[test]
fn parse_psql_bool_line_accepts_t_and_f() {
    assert_eq!(parse_psql_bool_line("t"), Ok(true));
    assert_eq!(parse_psql_bool_line("f"), Ok(false));
    assert_eq!(parse_psql_bool_line(" t\n"), Ok(true));
    assert!(parse_psql_bool_line("x").is_err());
    assert!(parse_psql_bool_line("").is_err());
}

#[test]
fn parse_relay_line_parses_enabled_and_bindings() {
    assert_eq!(parse_relay_line("t|2"), Ok((true, 2)));
    assert_eq!(parse_relay_line("f|0"), Ok((false, 0)));
    assert!(parse_relay_line("t").is_err());
    assert!(parse_relay_line("x|2").is_err());
    assert!(parse_relay_line("").is_err());
}

#[test]
fn parse_probe_line_resolves_candidates_in_order() {
    assert_eq!(
        parse_probe_line("audit_governance_outbox|"),
        Some("audit_governance_outbox")
    );
    assert_eq!(parse_probe_line("|audit_outbox"), Some("audit_outbox"));
    assert_eq!(parse_probe_line("|"), None);
    assert_eq!(parse_probe_line(""), None);
    assert_eq!(parse_probe_line("some_other_table|"), None);
}

#[test]
fn parse_buckets_fills_missing_statuses_with_zero() {
    assert_eq!(parse_buckets("0|3\n2|10"), [3, 0, 10, 0]);
    assert_eq!(parse_buckets("3|1"), [0, 0, 0, 1]);
    assert_eq!(parse_buckets(""), [0, 0, 0, 0]);
    assert_eq!(parse_buckets("0|1\n1|2\n2|3\n3|4"), [1, 2, 3, 4]);
}

#[test]
fn parse_v1_line_parses_three_buckets() {
    assert_eq!(
        parse_v1_line("1|0|0").unwrap(),
        V1OutboxCounts {
            pending: 1,
            claimed: 0,
            delivered: 0,
        }
    );
    assert_eq!(
        parse_v1_line("3|2|10").unwrap(),
        V1OutboxCounts {
            pending: 3,
            claimed: 2,
            delivered: 10,
        }
    );
    assert!(parse_v1_line("").is_err());
    assert!(parse_v1_line("1|0").is_err());
    assert!(parse_v1_line("x|0|0").is_err());
}

#[test]
fn parse_dead_rows_splits_error_text_with_pipes() {
    let rows = parse_dead_rows("uuid-1|403 refusal\nuuid-2|error with | pipe\n");
    assert_eq!(
        rows,
        vec![
            ("uuid-1".to_string(), "403 refusal".to_string()),
            ("uuid-2".to_string(), "error with | pipe".to_string()),
        ]
    );
    assert!(parse_dead_rows("").is_empty());
}

// -- URL parsing (FM6) --

#[test]
fn parse_db_url_full() {
    let c = parse_db_url("postgres://user:pass@dbhost:5433/mydb").unwrap();
    assert_eq!(
        c,
        ConnParams {
            user: "user".into(),
            password: "pass".into(),
            host: "dbhost".into(),
            port: "5433".into(),
            db: "mydb".into(),
        }
    );
}

#[test]
fn parse_db_url_defaults_and_no_password() {
    let c = parse_db_url("postgres://u@h/db").unwrap();
    assert_eq!(c.password, "");
    assert_eq!(c.host, "h");
    assert_eq!(c.port, "5432");
    let c = parse_db_url("postgres://u:p@/db").unwrap();
    assert_eq!(c.host, "localhost");
    assert_eq!(c.port, "5432");
}

#[test]
fn parse_db_url_strips_query_string() {
    let c = parse_db_url("postgres://u:p@h:5432/db?sslmode=disable").unwrap();
    assert_eq!(c.db, "db");
}

#[test]
fn parse_db_url_rejects_missing_parts() {
    assert!(parse_db_url("http://u:p@h/db").is_err());
    assert!(parse_db_url("postgres://u:p@h").is_err());
    assert!(parse_db_url("postgres://:p@h/db").is_err());
    assert!(parse_db_url("").is_err());
}

// -- regression: verdict snapshot wiring (A1) --

#[test]
fn run_without_url_is_handled_by_command_layer() {
    // The command layer (main.rs) pre-checks the URL env vars; `run`
    // itself must still fail closed on a malformed URL.
    let o = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(run("not-a-url"));
    assert!(o.is_error());
    assert_eq!(o.exit_code(), 1);
    assert!(o.message().contains("postgres://"));
}

// -- D8′ destructive gate (priority-drill-destructive-gate) --

#[test]
fn parse_outbox_count_accepts_psql_single_value() {
    // psql -At single-value contract: COUNT(*)::bigint never yields empty
    // output on success — "0" minimum.
    assert_eq!(parse_outbox_count("0"), Ok(0));
    assert_eq!(parse_outbox_count("501"), Ok(501));
    assert_eq!(parse_outbox_count(" 12\n"), Ok(12));
}

#[test]
fn parse_outbox_count_rejects_garbage() {
    // Garbage/empty output ⇒ psql failed ⇒ Err ⇒ fail-closed (never spawn).
    assert!(parse_outbox_count("").is_err());
    assert!(parse_outbox_count("abc").is_err());
    assert!(parse_outbox_count("1|2").is_err());
    assert!(parse_outbox_count("501.5").is_err());
    assert!(parse_outbox_count("t").is_err());
}

#[test]
fn priority_drill_blocked_matrix() {
    // Non-empty outbox + no opt-in → blocked.
    assert!(priority_drill_blocked(1, false));
    assert!(priority_drill_blocked(501, false));
    // Empty outbox never blocks.
    assert!(!priority_drill_blocked(0, false));
    assert!(!priority_drill_blocked(0, true));
    // Opt-in allows even a non-empty outbox.
    assert!(!priority_drill_blocked(1, true));
    assert!(!priority_drill_blocked(501, true));
    // Negative count is unreachable from SQL (CHECK status, COUNT >= 0) —
    // defensive: never block on it.
    assert!(!priority_drill_blocked(-1, false));
}
