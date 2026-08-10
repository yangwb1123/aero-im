//! Tests only — types and functions moved to `mod.rs`.
//!
//! Part of `REFACTOR_PLAN.md` Step 2.

// Re-export items from parent so sub-modules using `crate::message::orig::*` still work.
pub(crate) use super::{
    attached_blob_ids, hnsw_ef_search, searchable_of, MessageRow, ScoredMessageRow,
    EXPORT_SENDER_CAP,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hnsw_ef_search_widens_and_bounds_the_candidate_walk() {
        // Default: 4× limit, floored at 100, capped at 400.
        // Requesting 5 rows → 5*4=20 but floor is 100 → 100.
        assert_eq!(hnsw_ef_search(5), 100);
        // Requesting 30 rows → 30*4=120.
        assert_eq!(hnsw_ef_search(30), 120);
        // Cap: 200 rows → 200*4=800 but cap at 400 → 400.
        assert_eq!(hnsw_ef_search(200), 400);
        // Floor: 0 rows → 0*4=0 → floor at 100 → 100.
        assert_eq!(hnsw_ef_search(0), 100);
    }

    #[test]
    fn attached_blob_ids_extracts_file_and_voice_blobs_only() {
        // Use BlobId directly so serialization is correct.
        let b1 = aero_common::BlobId::new();
        let b2 = aero_common::BlobId::new();
        // Build JSON from serialized Block values to ensure correct format.
        let blocks_val = serde_json::to_value(vec![
            aero_common::Block::text("hello"),
            aero_common::Block::File {
                blob_id: b2,
                kind: aero_common::FileKind::Document,
                size: 100,
                name: "f.txt".into(),
            },
            aero_common::Block::Voice {
                blob_id: b1,
                transcript: None,
                duration_ms: 5000,
            },
        ])
        .unwrap();
        let ids = attached_blob_ids(&blocks_val);
        assert_eq!(ids.len(), 2, "should find both file and voice blob ids");
        assert!(ids.contains(&b1));
        assert!(ids.contains(&b2));
    }

    #[test]
    fn attached_blob_ids_empty_for_text_only_and_garbage() {
        assert!(
            attached_blob_ids(&serde_json::json!([{ "type": "text", "content": "hi" }])).is_empty()
        );
        assert!(attached_blob_ids(&serde_json::Value::Null).is_empty());
        assert!(attached_blob_ids(&serde_json::json!("not an array")).is_empty());
    }

    /// Gate round-3 B1 (failing-test-first): the recall history snapshot must
    /// never carry byte references (`blob_id`) — the recall tx enqueues those
    /// blobs for GC, and `message_edits` is invisible to the GC live-reference
    /// scan, so a snapshot with `blob_id`s would point at destroyed bytes.
    #[test]
    fn redact_blocks_for_recall_snapshot_removes_byte_references() {
        use aero_common::{BlobId, FileKind};
        use super::super::redact_blocks_for_recall_snapshot;

        let file_blob = BlobId::new();
        let voice_blob = BlobId::new();
        let blocks = vec![
            aero_common::Block::text("keep me"),
            aero_common::Block::File {
                blob_id: file_blob,
                kind: FileKind::Document,
                name: "f.txt".into(),
                size: 100,
            },
            aero_common::Block::Voice {
                blob_id: voice_blob,
                duration_ms: 5000,
                transcript: Some("spoken words".into()),
            },
            aero_common::Block::Voice {
                blob_id: BlobId::new(),
                duration_ms: 3000,
                transcript: None,
            },
        ];

        let redacted = redact_blocks_for_recall_snapshot(&blocks);

        // File → marker text, no byte reference.
        assert!(
            matches!(&redacted[1], aero_common::Block::Text { content, .. } if content == "[附件已移除]"),
            "file block becomes the attachment-removed marker"
        );
        // Voice with transcript → the transcript survives as text evidence.
        assert!(
            matches!(&redacted[2], aero_common::Block::Text { content, .. } if content == "spoken words"),
            "voice transcript is preserved as text"
        );
        // Voice without transcript → marker.
        assert!(
            matches!(&redacted[3], aero_common::Block::Text { content, .. } if content == "[语音已移除]"),
            "voice without transcript becomes the voice-removed marker"
        );
        // Non-attachment blocks pass through untouched.
        assert_eq!(
            serde_json::to_value(&redacted[0]).unwrap(),
            serde_json::to_value(aero_common::Block::text("keep me")).unwrap()
        );

        // The serialized snapshot must contain no byte reference at all.
        let json = serde_json::to_value(&redacted).unwrap();
        assert!(
            !json.to_string().contains(&file_blob.to_string()),
            "file blob_id must not leak into the snapshot"
        );
        assert!(
            !json.to_string().contains(&voice_blob.to_string()),
            "voice blob_id must not leak into the snapshot"
        );
        assert_eq!(json.to_string().matches("\"blob_id\"").count(), 0);
    }
}

// ---------- PG-gated db_tests ----------
// These require a live DB to run. They live here so the orig module is still
// complete (eventually they'll move to their sub-module tests).
#[cfg(test)]
mod db_tests {
    use crate::MessageRepo;
    use aero_common::{MessageId, ParticipantId, RoomId};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("msg-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,'00000000-0000-0000-0000-000000000000'::uuid)",
        )
        .bind(id.to_uuid())
        .bind(format!("msg-room-{id}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("join");
        id
    }

    async fn insert_msg(p: &PgPool, room: RoomId, sender: ParticipantId) -> MessageId {
        let id = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks) VALUES ($1,$2,$3,'[]'::jsonb)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(p)
        .await
        .expect("insert msg");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn expired_ephemeral_is_hidden_from_reads_before_sweep() {
        // An ephemeral message past its TTL must be invisible to reads the instant
        // it lapses — NOT only after the periodic sweep hard-deletes it (which can
        // be up to an hour later). Read-side `expires_at > now()` filtering enforces
        // that without running the sweep.
        let p = pool();
        let u = participant(&p).await;
        let r = room(&p, u).await;
        let repo = MessageRepo::new(p.clone());

        let normal = insert_msg(&p, r, u).await;
        // An already-expired ephemeral (expires_at 1 minute in the PAST), inserted
        // directly; the sweep has NOT run.
        let expired = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, expires_at)
             VALUES ($1,$2,$3,'[]'::jsonb, now() - interval '1 minute')",
        )
        .bind(expired.to_uuid())
        .bind(r.to_uuid())
        .bind(u.to_uuid())
        .execute(&p)
        .await
        .expect("insert expired ephemeral");
        // A still-live ephemeral (expires in the future) must remain visible.
        let live = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, expires_at)
             VALUES ($1,$2,$3,'[]'::jsonb, now() + interval '1 hour')",
        )
        .bind(live.to_uuid())
        .bind(r.to_uuid())
        .bind(u.to_uuid())
        .execute(&p)
        .await
        .expect("insert live ephemeral");

        let ids: Vec<_> = repo
            .list_recent(r, None, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(ids.contains(&normal), "normal message visible");
        assert!(ids.contains(&live), "unexpired ephemeral visible");
        assert!(
            !ids.contains(&expired),
            "EXPIRED ephemeral hidden from reads before the sweep"
        );

        // The same holds for the reconnect-backfill path (list_since).
        let since: Vec<_> = repo
            .list_since(r, MessageId::from_uuid(uuid::Uuid::nil()), 50)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(
            !since.contains(&expired),
            "expired ephemeral not replayed on backfill"
        );
        assert!(since.contains(&live), "live ephemeral replayed");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn messages_around_has_more_reflects_either_side() {
        // Few messages BEFORE the target, many AFTER: `has_more` must be true
        // (the after-side window truncated) — the bug only checked the before side
        // and would report false, hiding the unloaded newer messages.
        let p = pool();
        let u = participant(&p).await;
        let r = room(&p, u).await;
        let repo = MessageRepo::new(p.clone());

        insert_msg(&p, r, u).await; // 1 before
        let target = insert_msg(&p, r, u).await;
        for _ in 0..5 {
            insert_msg(&p, r, u).await; // 5 after
        }

        let (rows, has_more) = repo.messages_around(r, target, 3).await.expect("around");
        assert!(
            has_more,
            "after-side truncation (5 > window 3) must set has_more"
        );
        // before(1, capped at 3) + target(1) + after(3, capped) = 5 rows returned.
        assert_eq!(
            rows.len(),
            5,
            "1 before + target + 3 after (capped): {}",
            rows.len()
        );
        assert!(
            rows.iter().any(|m| m.id == target),
            "the target itself is included"
        );

        // A tight window where neither side truncates → has_more false.
        let (_rows2, has_more2) = repo.messages_around(r, target, 100).await.expect("around2");
        assert!(!has_more2, "neither side truncated with a generous window");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_id_ordering_is_chronological() {
        let p = pool();
        let u = participant(&p).await;
        let r = room(&p, u).await;
        let repo = MessageRepo::new(p.clone());

        // Insert two messages; list_recent returns newest first.
        let m1 = insert_msg(&p, r, u).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let m2 = insert_msg(&p, r, u).await;

        let recent = repo.list_recent(r, None, 10).await.expect("list_recent");
        assert_eq!(recent.len(), 2, "both messages returned");
        assert_eq!(recent[0].id, m2, "newest first");
        assert_eq!(recent[1].id, m1, "oldest second");

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE id IN ($1,$2)")
            .bind(m1.to_uuid())
            .bind(m2.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(u.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn has_embedding_reflects_stored_vector() {
        let p = pool();
        let u = participant(&p).await;
        let r = room(&p, u).await;
        let repo = MessageRepo::new(p.clone());

        // A freshly inserted message has no embedding yet.
        let m = insert_msg(&p, r, u).await;
        assert!(
            !repo.has_embedding(m).await.expect("has_embedding"),
            "new message starts un-embedded"
        );

        // A never-inserted id reports false (missing ⇒ not embedded).
        assert!(
            !repo
                .has_embedding(MessageId::new())
                .await
                .expect("has_embedding missing"),
            "missing id reports not-embedded"
        );

        // After storing an embedding it flips to true.
        let dim = 1024usize;
        repo.update_embedding(m, vec![0.0_f32; dim])
            .await
            .expect("update_embedding");
        assert!(
            repo.has_embedding(m).await.expect("has_embedding after"),
            "stored embedding is detected"
        );

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(m.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(u.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn unread_counts_by_room_returns_correct_counts() {
        let p = pool();
        let u = participant(&p).await;
        let other = participant(&p).await;
        let r = room(&p, u).await;

        // Join the second participant so they're a room member too.
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(r.to_uuid())
        .bind(other.to_uuid())
        .execute(&p)
        .await
        .expect("join other");

        let repo = MessageRepo::new(p.clone());

        // Insert messages from 'other' (not u — unread counts exclude own messages).
        let m1 = insert_msg(&p, r, other).await;
        let m2 = insert_msg(&p, r, other).await;

        let counts = repo.unread_counts_by_room(u).await.expect("unread_counts");
        let my_count: u32 = counts
            .iter()
            .find(|(rid, _)| *rid == r)
            .map_or(0, |(_, c)| *c);
        assert!(
            my_count >= 2,
            "should see at least 2 unread: got {my_count}"
        );

        // Mark one as read via receipt.
        sqlx::query("INSERT INTO read_receipts (room_id, participant_id, last_read_message_id) VALUES ($1,$2,$3) ON CONFLICT (room_id, participant_id) DO UPDATE SET last_read_message_id = EXCLUDED.last_read_message_id")
            .bind(r.to_uuid())
            .bind(u.to_uuid())
            .bind(m1.to_uuid())
            .execute(&p)
            .await
            .expect("mark read");

        let counts2 = repo.unread_counts_by_room(u).await.expect("unread_counts2");
        let remaining: u32 = counts2
            .iter()
            .find(|(rid, _)| *rid == r)
            .map_or(0, |(_, c)| *c);
        assert!(remaining < my_count, "should decrease after marking read");

        // Cleanup.
        sqlx::query("DELETE FROM read_receipts WHERE room_id = $1 AND participant_id = $2")
            .bind(r.to_uuid())
            .bind(u.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id IN ($1,$2)")
            .bind(m1.to_uuid())
            .bind(m2.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(r.to_uuid())
            .bind(u.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id IN ($1,$2)")
            .bind(u.to_uuid())
            .bind(other.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
