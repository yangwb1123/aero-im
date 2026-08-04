use super::*;

#[test]
fn attach_and_read_usage_round_trips() {
    let mut result = serde_json::json!({ "answer": "x" });
    attach_usage(
        &mut result,
        Some(Usage {
            input_tokens: 100,
            output_tokens: 25,
        }),
    );
    assert_eq!(result["usage"]["input_tokens"], 100);
    assert_eq!(result["usage"]["output_tokens"], 25);
    let usage = usage_from_result(&result).expect("usage present");
    assert_eq!(
        usage,
        Usage {
            input_tokens: 100,
            output_tokens: 25
        }
    );
}

#[test]
fn attach_usage_none_is_noop_and_reads_back_none() {
    let mut result = serde_json::json!({ "summary": "x" });
    attach_usage(&mut result, None);
    assert!(result.get("usage").is_none());
    assert!(usage_from_result(&result).is_none());
}

#[tokio::test]
async fn run_one_records_real_token_cost_when_usage_present() {
    let job = mk_job(AiJobKind::Answer, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({
            "answer": "42",
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
        }),
    };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost, test_usage_sink()).await;

    let want = cost.token_micros(1000, 100);
    assert_ne!(want, cost.answer_micros);
    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
        )),
        "expected real token cost {want}, not the flat estimate:\n{out}"
    );
}

#[tokio::test]
async fn run_one_labels_cost_with_job_workspace() {
    let ws = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0099);
    let mut job = mk_job(AiJobKind::Answer, 1);
    job.workspace_id = Some(ws);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({
            "answer": "42",
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
        }),
    };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost, test_usage_sink()).await;

    let want = cost.token_micros(1000, 100);
    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer",workspace="{ws}"}} {want}"#
        )),
        "per-workspace cost not recorded under the job's workspace:\n{out}"
    );
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
        )),
        "aggregate cost series must be preserved:\n{out}"
    );
}

#[tokio::test]
async fn run_one_falls_back_to_estimate_for_paid_provider_without_usage() {
    let job = mk_job(AiJobKind::Summarize, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({ "summary": "x", "anthropic": true }),
    };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost, test_usage_sink()).await;

    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="summarize"}} {}"#,
            cost.summarize_micros
        )),
        "expected the flat estimate fallback:\n{out}"
    );
}

#[tokio::test]
async fn run_one_no_key_fallback_records_zero_cost() {
    let job = mk_job(AiJobKind::Summarize, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({ "summary": "x", "anthropic": false }),
    };
    let reg = test_reg();

    run_one(
        &queue,
        &proc,
        job,
        &reg,
        &CostModel::default(),
        test_usage_sink(),
    )
    .await;

    assert!(reg
        .render_prometheus()
        .contains(r#"aero_ai_cost_micros_total{kind="summarize"} 0"#));
}

#[tokio::test]
async fn run_one_paid_success_without_usage_sink_is_retried_not_completed() {
    let job = mk_job(AiJobKind::Answer, 1);
    let id = job.id;
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({ "answer": "paid", "anthropic": true }),
    };
    let reg = test_reg();

    run_one(&queue, &proc, job, &reg, &CostModel::default(), None).await;

    assert_eq!(queue.status_of(id), AiJobStatus::Queued);
    let rows = queue.rows.lock().unwrap();
    let error = rows[0].job.error.as_deref().unwrap_or_default();
    assert!(error.contains("usage accounting failed"));
    let metrics = reg.render_prometheus();
    assert!(metrics.contains(r#"aero_ai_jobs_total{kind="answer",outcome="failure"} 1"#));
    assert!(!metrics.contains(r#"aero_ai_jobs_total{kind="answer",outcome="success"}"#));
}

#[tokio::test]
async fn run_one_embed_skip_records_zero_cost_but_still_succeeds() {
    let job = mk_job(AiJobKind::Embed, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    queue.rows.lock().unwrap()[0].status = AiJobStatus::Running;
    let proc = FixedResult {
        value: serde_json::json!({ "skipped": "empty", "updated": false }),
    };
    let reg = test_reg();

    run_one(&queue, &proc, job, &reg, &test_cost(), test_usage_sink()).await;

    let out = reg.render_prometheus();
    assert!(out.contains(r#"aero_ai_jobs_total{kind="embed",outcome="success"} 1"#));
    assert!(out.contains(r#"aero_ai_cost_micros_total{kind="embed"} 0"#));
    assert!(out.contains(r#"aero_ai_job_duration_seconds_count{kind="embed"} 1"#));
}
