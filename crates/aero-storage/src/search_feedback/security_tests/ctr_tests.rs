use super::*;

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn ctr_uses_all_proof_impressions_as_its_denominator() {
    let fixture = Fixture::new("search-proof-ctr").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());

    let top = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "ctr top",
            &[fixture.first, fixture.second],
        )
        .await
        .unwrap();
    repo.record_impression_click(fixture.actor, top.id, fixture.first)
        .await
        .unwrap();

    let lower = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "ctr lower",
            &[fixture.first, fixture.second],
        )
        .await
        .unwrap();
    repo.record_impression_click(fixture.actor, lower.id, fixture.second)
        .await
        .unwrap();

    repo.create_impression(
        fixture.actor,
        fixture.workspace,
        "ctr no click",
        &[fixture.first],
    )
    .await
    .unwrap();

    let stats = repo.ctr_stats(fixture.workspace, 30).await.unwrap();
    assert_eq!(stats.impressions, 3);
    assert_eq!(stats.clicks, 2);
    assert_eq!(stats.queries, 2);
    assert!((stats.mean_reciprocal_rank - 0.75).abs() < 1e-9);
    assert!((stats.click_through_rate - (2.0 / 3.0)).abs() < 1e-9);
    assert!(
        (stats.top_result_ctr - (1.0 / 3.0)).abs() < 1e-9,
        "top-result CTR is top clicks / all impressions, not top clicks / clicks"
    );

    fixture.cleanup().await;
}
