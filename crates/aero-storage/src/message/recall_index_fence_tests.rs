//! DB-integration tests for the recall index fence (gate F1, security review
//! stage 02/06): a worker that READ a message pre-recall must never be able to
//! write the ORIGINAL text/vector post-recall. The three lockless index
//! UPDATEs (`update_embedding`, `update_searchable_text`,
//! `update_voice_transcript`) fence `recalled_at` in their `WHERE` clause —
//! exactly like the recall UPDATE itself — so each post-recall write is a
//! no-op and the cleared index state survives: empty `searchable_text` (FTS
//! `search_tsv` is a STORED generated column), NULL `embedding` (vector
//! search), placeholder blocks, no version bump.
//!
//! The two TRANSACTIONAL system writers (`edit_outboxed_system` — the unfurl
//! bot's path — and `update_voice_transcript_outboxed` — the transcribe bot's
//! production path) have no SQL WHERE fence: they serialize with recall on the
//! row lock (`FOR UPDATE`) and re-check `recalled_at` in Rust. Their race
//! coverage lives here too (`concurrent_recall_vs_system_edit_never_resurrects`
//! / `concurrent_recall_vs_transcript_write_never_resurrects`): under ANY
//! interleaving the final state must be the recalled placeholder with cleared
//! index columns.
//!
//! Reuses the recall `Fixture` (workspace/room/author) from the sibling
//! `recall_tests` module.
//!
//! Run with a live Postgres with migrations applied:
//!
//! ```text
//! DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
//!   cargo test -p aero-storage --lib -- --ignored message::recall
//! ```

use aero_common::{Block, RECALLED_MESSAGE_PLACEHOLDER};

use super::recall_tests::Fixture;
use super::MessageRepo;

/// Deterministic repro: recall first, then every pre-recall worker write must
/// be refused by the WHERE fence (`false` / `None`), and the cleared index
/// state survives.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_fences_late_index_writes() {
    let fixture = Fixture::create("late-index").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture
        .insert_message(fixture.author, "pre-recall index body")
        .await;

    let recalled = repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    // The AI worker's late writes (embed / doc-fold / transcribe) must all be
    // refused by the WHERE fence — a pre-recall read is never authority.
    assert!(
        !repo
            .update_embedding(message.id, vec![0.5_f32; 1024])
            .await
            .unwrap(),
        "embedding write after recall must be a no-op"
    );
    assert!(
        !repo
            .update_searchable_text(message.id, "resurrected original text")
            .await
            .unwrap(),
        "searchable_text write after recall must be a no-op"
    );
    assert!(
        repo.update_voice_transcript(message.id, "late transcript")
            .await
            .unwrap()
            .is_none(),
        "voice-transcript write after recall must be a no-op"
    );

    // The recall's cleared index state survives all three late writes.
    let (searchable, embedding): (String, Option<pgvector::Vector>) =
        sqlx::query_as("SELECT searchable_text, embedding FROM messages WHERE id = $1")
            .bind(message.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(searchable.is_empty(), "FTS source stays cleared");
    assert!(embedding.is_none(), "vector stays cleared");
    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
        "placeholder body survives the late index writes"
    );
    assert_eq!(
        stored.version, recalled.message.version,
        "no version bump from fenced writes"
    );

    fixture.cleanup().await;
}

/// Race flavor: recall and a pre-recall index worker write racing on the same
/// row must NEVER leave a recalled row holding index content. The atomic WHERE
/// fence (`recalled_at IS NULL`) guarantees the invariant under any
/// interleaving; without it, a write landing after the recall's index clear
/// would re-populate `embedding`/`searchable_text` and the assertion fails.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_recall_vs_embed_write_never_resurrects() {
    let fixture = Fixture::create("index-race").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    for round in 0..8 {
        let message = fixture
            .insert_message(fixture.author, &format!("race secret {round}"))
            .await;
        let folded_text = format!("folded doc {round}");
        let recall =
            repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None);
        let embed = repo.update_embedding(message.id, vec![0.5_f32; 1024]);
        let fold = repo.update_searchable_text(message.id, &folded_text);
        let (recalled, embedded, folded) = tokio::join!(recall, embed, fold);
        assert!(
            recalled.unwrap().is_some(),
            "recall wins on a fresh message"
        );
        let _ = embedded.unwrap();
        let _ = folded.unwrap();

        let (searchable, embedding): (String, Option<pgvector::Vector>) =
            sqlx::query_as("SELECT searchable_text, embedding FROM messages WHERE id = $1")
                .bind(message.id.to_uuid())
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert!(
            searchable.is_empty(),
            "round {round}: recalled row must never carry FTS text"
        );
        assert!(
            embedding.is_none(),
            "round {round}: recalled row must never carry a vector"
        );
        let stored = repo.get(message.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&stored.blocks).unwrap(),
            serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
            "round {round}: placeholder body survives the race"
        );
    }

    fixture.cleanup().await;
}

/// Race flavor for the TRANSACTIONAL system-edit writer (the unfurl bot's
/// production path, `edit_outboxed_system` → `edit_locked_outboxed_in_tx`):
/// no SQL WHERE fence — it serializes with recall on the row lock and
/// re-checks `recalled_at` in Rust. Under ANY interleaving the final state
/// must be the recalled placeholder with cleared index columns:
///
/// * recall wins the lock → the edit observes `recalled_at` and returns
///   `None` (fenced);
/// * edit wins the lock first (its `expected_version` still matches) → the
///   edit commits, then recall replaces body + clears the index afterwards.
///
/// A refactor that drops the lock (or moves the re-check) resurrects content
/// and this test goes red.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_recall_vs_system_edit_never_resurrects() {
    let fixture = Fixture::create("edit-race").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    for round in 0..8 {
        let message = fixture
            .insert_message(fixture.author, &format!("edit race secret {round}"))
            .await;
        let recall =
            repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None);
        let edit = repo.edit_outboxed_system(
            message.id,
            vec![Block::text(format!("resurrected edit {round}"))],
            message.version,
            None,
            None,
        );
        let (recalled, edited) = tokio::join!(recall, edit);
        assert!(
            recalled.unwrap().is_some(),
            "round {round}: recall wins on a fresh message"
        );
        // Either the edit was fenced (None, recall held the lock first) or it
        // committed before the recall — both must converge to the placeholder.
        let _ = edited.unwrap();

        let (searchable, embedding): (String, Option<pgvector::Vector>) =
            sqlx::query_as("SELECT searchable_text, embedding FROM messages WHERE id = $1")
                .bind(message.id.to_uuid())
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert!(
            searchable.is_empty(),
            "round {round}: system edit must never resurrect FTS text"
        );
        assert!(
            embedding.is_none(),
            "round {round}: system edit must never resurrect a vector"
        );
        let stored = repo.get(message.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&stored.blocks).unwrap(),
            serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
            "round {round}: placeholder body survives the system-edit race"
        );
        assert!(stored.recalled_at.is_some());
    }

    fixture.cleanup().await;
}

/// Race flavor for the TRANSACTIONAL voice-transcript writer (the transcribe
/// bot's production path, `update_voice_transcript_outboxed`): no SQL WHERE
/// fence — row lock + Rust re-check. The message carries a `Voice` block with
/// `transcript: None` (the raw SQL blocks rewrite skips insert-time
/// attachment validation; the transcript writer only ever reads blocks under
/// its own row lock). Under ANY interleaving the final state must be the
/// recalled placeholder with cleared index columns.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_recall_vs_transcript_write_never_resurrects() {
    let fixture = Fixture::create("tx-race").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    for round in 0..8 {
        let message = fixture
            .insert_message(fixture.author, &format!("tx race secret {round}"))
            .await;
        // Morph the body into an untranscribed Voice block (direct SQL so the
        // nonexistent blob id never trips insert-time attachment validation).
        let voice_blocks = serde_json::json!([{
            "type": "voice",
            "blob_id": aero_common::BlobId::new().to_string(),
            "duration_ms": 1200,
            "transcript": null,
        }]);
        sqlx::query("UPDATE messages SET blocks = $1 WHERE id = $2")
            .bind(&voice_blocks)
            .bind(message.id.to_uuid())
            .execute(&fixture.pool)
            .await
            .unwrap();

        let recall =
            repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None);
        let late_transcript = format!("late transcript {round}");
        let transcribe = repo.update_voice_transcript_outboxed(message.id, &late_transcript, None);
        let (recalled, transcripted) = tokio::join!(recall, transcribe);
        assert!(
            recalled.unwrap().is_some(),
            "round {round}: recall wins on a fresh message"
        );
        // Either fenced (None) or committed before the recall — both converge.
        let _ = transcripted.unwrap();

        let (searchable, embedding): (String, Option<pgvector::Vector>) =
            sqlx::query_as("SELECT searchable_text, embedding FROM messages WHERE id = $1")
                .bind(message.id.to_uuid())
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert!(
            searchable.is_empty(),
            "round {round}: transcript write must never resurrect FTS text"
        );
        assert!(
            embedding.is_none(),
            "round {round}: transcript write must never resurrect a vector"
        );
        let stored = repo.get(message.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&stored.blocks).unwrap(),
            serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
            "round {round}: placeholder body survives the transcript race"
        );
        assert!(stored.recalled_at.is_some());
    }

    fixture.cleanup().await;
}
