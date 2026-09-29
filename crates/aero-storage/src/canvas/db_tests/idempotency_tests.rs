use super::*;

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn client_canvas_creation_identity_is_idempotent_and_scope_bound() {
    let p = pool();
    let repo = CanvasRepo::new(p.clone());
    let f = fixture(&p).await;
    let client_create_id = uuid::Uuid::now_v7();
    let blocks = serde_json::json!([{ "type": "text", "content": "first" }]);

    let first = repo
        .create_canvas_with_client_id_authorized(
            f.room,
            f.member,
            client_create_id,
            "Retry-safe create",
            &blocks,
        )
        .await
        .unwrap();
    let retry = repo
        .create_canvas_with_client_id_authorized(
            f.room,
            f.member,
            client_create_id,
            "Retry-safe create",
            &blocks,
        )
        .await
        .unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(first.id.to_uuid(), client_create_id);
    assert_eq!(retry.created_at, first.created_at);
    assert_eq!(
        repo.list_canvases_authorized(f.room, f.member)
            .await
            .unwrap()
            .iter()
            .filter(|canvas| canvas.id == first.id)
            .count(),
        1
    );
    assert!(matches!(
        repo.create_canvas_with_client_id_authorized(
            f.other_room,
            f.owner,
            client_create_id,
            "Cross-room collision",
            &blocks,
        )
        .await,
        Err(Error::Conflict(_))
    ));
}
