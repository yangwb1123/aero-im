use super::db_tests::{insert_match, participant, pool, room, workspace};
use super::process_monitored_search;
use aero_common::{Block, ParticipantId};
use aero_storage::{MessageRepo, MonitoredSearch, NewMessage, NotificationRepo, SavedSearchRepo};

/// A monitored search with a prior `last_run_at` notifies the owner of matches
/// created AFTER that cursor (and nothing for the first-ever run, which only
/// sets the baseline). Verifies the new-match → SavedSearch-notification path.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn notifies_owner_of_new_matches_since_last_run() {
    let p = pool();
    let notifications = NotificationRepo::new(p.clone());
    let saved = SavedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());

    let owner = participant(&p).await;
    let ws = workspace(&p, owner).await;
    let r = room(&p, owner, ws).await;
    let needle = format!("zqssmtoken{}", ParticipantId::new());

    // A matching message exists already (BEFORE the cursor → must NOT notify).
    let old = msgs
        .insert(NewMessage {
            room_id: r,
            sender_id: owner,
            blocks: vec![Block::text(format!("{needle} old one"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert old");

    let sid = saved
        .create(owner, ws, "mon", &needle)
        .await
        .expect("create");
    saved
        .set_notify_new(sid, owner, true)
        .await
        .expect("enable monitoring");

    // First-ever run: baseline only, zero notifications even though `old` matches.
    let item0 = MonitoredSearch {
        id: sid,
        owner,
        workspace: ws,
        query: needle.clone(),
        cursor_at: None,
        cursor_message_id: None,
    };
    let now0 = time::OffsetDateTime::now_utc();
    let n0 = process_monitored_search(&saved, &item0, now0, 20)
        .await
        .expect("baseline run");
    assert_eq!(
        n0, 0,
        "first-ever run only sets the baseline, notifies nothing"
    );
    saved
        .mark_run(sid, owner, now0 + time::Duration::days(1))
        .await
        .expect("manual saved-search run");

    // A NEW matching message uses the exact baseline timestamp. The baseline
    // stores a nil UUID tiebreaker, so this non-nil message must still enter
    // the next run rather than being lost at the timestamp boundary.
    let fresh = insert_match(&p, r, owner, &needle, now0).await;

    // Second run with the baseline as the cursor: notifies for the new match only.
    let item1 = MonitoredSearch {
        id: sid,
        owner,
        workspace: ws,
        query: needle.clone(),
        cursor_at: Some(now0),
        cursor_message_id: None,
    };
    let now1 = now0 + time::Duration::seconds(60);
    let n1 = process_monitored_search(&saved, &item1, now1, 20)
        .await
        .expect("delta run");
    assert_eq!(n1, 1, "exactly the one new-since-baseline match notifies");

    // The notification is a SavedSearch kind pointing at the fresh message.
    let inbox = notifications
        .list(owner, None, false, Some(50))
        .await
        .expect("inbox");
    assert!(
        inbox.iter().any(|n| n.message_id == fresh
            && matches!(n.kind, aero_common::NotificationKind::SavedSearch)),
        "a SavedSearch notification for the fresh match exists",
    );
    assert!(
        !inbox.iter().any(|n| n.message_id == old.id),
        "the pre-baseline match was never notified",
    );
}
