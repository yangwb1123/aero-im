use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

#[test]
fn query_normalization_is_bounded() {
    assert_eq!(
        normalize_search_query("  alpha \n beta  ").unwrap(),
        "alpha beta"
    );
    assert!(normalize_search_query(" \t ").is_err());
    assert!(normalize_search_query(&"x".repeat(MAX_SEARCH_QUERY_CHARS + 1)).is_err());
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn legacy_clicks_aggregate_without_integer_overflow() {
    let pool = pool();
    let repo = SearchFeedbackRepo::new(pool.clone());
    let workspace = WorkspaceId::new();
    let participant = ParticipantId::new();

    for (query, rank) in [("alpha", 0), ("alpha", 1), ("alpha", 3), ("beta", 0)] {
        repo.record_legacy_click(participant, workspace, query, MessageId::new(), rank)
            .await
            .unwrap();
    }
    sqlx::query(
        r"INSERT INTO search_click_events
             (participant_id, workspace_id, query_text, result_id, result_rank, clicked_at)
           VALUES ($1, $2, 'historical-overflow', $3, 2147483647,
                   clock_timestamp() - INTERVAL '1 day')",
    )
    .bind(participant.to_uuid())
    .bind(workspace.to_uuid())
    .bind(MessageId::new().to_uuid())
    .execute(&pool)
    .await
    .expect_err("new rows must obey the rank bound");

    let stats = repo.ctr_stats(workspace, 30).await.unwrap();
    assert_eq!(
        stats.impressions, 0,
        "legacy clicks have no trustworthy impression denominator"
    );
    assert_eq!(stats.clicks, 4);
    assert_eq!(stats.queries, 2);
    assert!((stats.mean_reciprocal_rank - 0.6875).abs() < 1e-9);
    assert!(stats.click_through_rate.abs() < f64::EPSILON);
    assert!(stats.top_result_ctr.abs() < f64::EPSILON);

    sqlx::query("DELETE FROM search_click_events WHERE workspace_id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
}
