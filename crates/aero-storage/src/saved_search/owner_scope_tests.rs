use super::db_tests::{default_ws, owner, pool};
use super::*;

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn saved_search_create_list_get_delete_owner_scoped() {
    let p = pool();
    let repo = SavedSearchRepo::new(p.clone());
    let ws = default_ws();
    let owner = owner(&p).await;
    let stranger = ParticipantId::new();

    // create → list shows it.
    let id = repo
        .create(owner, ws, "deploys", "deploy failed")
        .await
        .unwrap();
    let other = repo
        .create(owner, ws, "incidents", "incident open")
        .await
        .unwrap();
    let listed = repo.list_for(owner, ws, None, 100).await.unwrap();
    assert!(
        listed.iter().any(|s| s.id == id),
        "list shows the saved search"
    );
    let found = listed.iter().find(|s| s.id == id).expect("present");
    assert_eq!(found.name, "deploys");
    assert_eq!(found.query, "deploy failed");
    let first_page = repo.list_for(owner, ws, None, 1).await.unwrap();
    assert_eq!(first_page.len(), 1, "requested page size is enforced");
    let second_page = repo
        .list_for(owner, ws, Some(first_page[0].id), 1)
        .await
        .unwrap();
    assert_eq!(second_page.len(), 1, "keyset cursor advances the page");
    assert_ne!(first_page[0].id, second_page[0].id);
    assert!(
        [id, other].contains(&second_page[0].id),
        "cursor stays inside the owner's workspace"
    );
    assert!(repo
        .list_for(stranger, ws, Some(first_page[0].id), 100)
        .await
        .unwrap()
        .is_empty());

    // get (owner) works; get (stranger) is None.
    let got = repo.get(id, owner).await.unwrap().expect("owner can get");
    assert_eq!(got.id, id);
    assert_eq!(got.query, "deploy failed");
    assert!(
        repo.get(id, stranger).await.unwrap().is_none(),
        "stranger cannot get another user's saved search"
    );

    // A stranger's delete is a no-op; the owner's first delete succeeds, the
    // second is a no-op.
    assert!(
        !repo.delete(id, stranger).await.unwrap(),
        "stranger cannot delete another user's saved search"
    );
    assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
    assert!(
        !repo.delete(id, owner).await.unwrap(),
        "second delete is a no-op"
    );
    assert!(
        !repo
            .list_for(owner, ws, None, 100)
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "deleted saved search leaves the list"
    );

    // Cleanup so reruns stay self-contained.
    sqlx::query("DELETE FROM saved_searches WHERE participant_id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .ok();
}
