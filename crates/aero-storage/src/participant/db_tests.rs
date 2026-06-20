//! DB-integration tests for [`super::ParticipantRepo`], split out of
//! participant.rs to keep that file under the 1200-line HARD limit.
    use super::ParticipantRepo;
    use crate::message::{MessageRepo, NewMessage};
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
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
            .bind(format!("erasure-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(id.to_uuid())
        .bind(format!("erasure-room-{id}"))
        .bind(creator.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    /// GDPR right-to-erasure must null the pgvector `embedding` alongside
    /// blocks/searchable_text — a retained semantic vector is re-identifiable.
    /// Guards the participant.rs erasure UPDATE against dropping `embedding = NULL`.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn erasure_nulls_message_embedding() {
        let p = pool();
        let messages = MessageRepo::new(p.clone());
        let participants = ParticipantRepo::new(p.clone());

        let sender = participant(&p).await;
        let r = room(&p, sender).await;

        let msg = messages
            .insert(NewMessage {
                room_id: r,
                sender_id: sender,
                blocks: vec![Block::text("a private secret to be erased")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert message");

        // Populate a non-null embedding, mirroring what AiWorker would store.
        assert!(
            messages.update_embedding(msg.id, vec![0.25_f32; 1024]).await.expect("set embedding"),
            "embedding should be set before erasure",
        );

        assert!(participants.delete_participant(sender).await.expect("erase"));

        let (searchable, embedding_is_null): (String, bool) = sqlx::query_as(
            "SELECT searchable_text, embedding IS NULL FROM messages WHERE id = $1",
        )
        .bind(msg.id.to_uuid())
        .fetch_one(&p)
        .await
        .expect("reload erased message");

        assert_eq!(searchable, "", "searchable_text cleared by erasure");
        assert!(embedding_is_null, "embedding must be nulled by erasure (re-identifiable)");
    }

    /// A message in a room under an active legal hold survives GDPR erasure
    /// (Art. 17(3)(e)); a message in an unheld room is erased. Guards the erasure
    /// UPDATE's legal-hold exemption (mirrors the retention sweep).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn erasure_exempts_legal_held_messages() {
        let p = pool();
        let messages = MessageRepo::new(p.clone());
        let participants = ParticipantRepo::new(p.clone());

        let sender = participant(&p).await;
        let held_room = room(&p, sender).await;
        let free_room = room(&p, sender).await;

        let held = messages
            .insert(NewMessage {
                room_id: held_room,
                sender_id: sender,
                blocks: vec![Block::text("preserved under legal hold")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("held msg");
        let free = messages
            .insert(NewMessage {
                room_id: free_room,
                sender_id: sender,
                blocks: vec![Block::text("erase me")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("free msg");

        // Place an active legal hold over `held_room` (default all-zero workspace).
        sqlx::query(
            "INSERT INTO legal_holds (id, workspace_id, room_id, reason, created_by) \
             VALUES ($1, $2, $3, 'eDiscovery', $4)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .bind(held_room.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("place hold");

        assert!(participants.delete_participant(sender).await.expect("erase"));

        let held_text: String =
            sqlx::query_scalar("SELECT searchable_text FROM messages WHERE id = $1")
                .bind(held.id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("reload held");
        let free_text: String =
            sqlx::query_scalar("SELECT searchable_text FROM messages WHERE id = $1")
                .bind(free.id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("reload free");

        assert_eq!(held_text, "preserved under legal hold", "held message must NOT be erased");
        assert_eq!(free_text, "", "unheld message must be erased");
    }

    /// After a legal hold releases, the deferred-erasure sweep completes erasure of
    /// a deleted participant's previously-exempt messages. No-op while the hold is
    /// active; idempotent once done.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn deferred_sweep_erases_after_hold_release() {
        let p = pool();
        let messages = MessageRepo::new(p.clone());
        let participants = ParticipantRepo::new(p.clone());

        let sender = participant(&p).await;
        let held_room = room(&p, sender).await;
        let msg = messages
            .insert(NewMessage {
                room_id: held_room,
                sender_id: sender,
                blocks: vec![Block::text("held then released")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("msg");

        let hold_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO legal_holds (id, workspace_id, room_id, reason, created_by) \
             VALUES ($1, $2, $3, 'hold', $4)",
        )
        .bind(hold_id)
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .bind(held_room.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("hold");

        // Erase the account: the held message is exempt, so it keeps its content,
        // and the deferred sweep is a no-op while the hold is active.
        assert!(participants.delete_participant(sender).await.expect("erase"));
        assert_eq!(
            participants.sweep_deferred_erasure().await.expect("sweep held"),
            0,
            "nothing erased while the hold is active",
        );
        let text: String = sqlx::query_scalar("SELECT searchable_text FROM messages WHERE id = $1")
            .bind(msg.id.to_uuid())
            .fetch_one(&p)
            .await
            .expect("reload");
        assert_eq!(text, "held then released", "still preserved under active hold");

        // Release the hold → the sweep completes erasure, and is then idempotent.
        sqlx::query("UPDATE legal_holds SET active = false, released_at = now() WHERE id = $1")
            .bind(hold_id)
            .execute(&p)
            .await
            .expect("release");
        assert_eq!(
            participants.sweep_deferred_erasure().await.expect("sweep released"),
            1,
            "one message erased after hold release",
        );
        let text: String = sqlx::query_scalar("SELECT searchable_text FROM messages WHERE id = $1")
            .bind(msg.id.to_uuid())
            .fetch_one(&p)
            .await
            .expect("reload2");
        assert_eq!(text, "", "erased after hold release");
        assert_eq!(
            participants.sweep_deferred_erasure().await.expect("sweep idem"),
            0,
            "deferred erasure is idempotent",
        );
    }

    /// GDPR erasure must remove the participant's OWN identity PII: tombstone
    /// display_name + clear avatar_url, and hard-delete credentials (login email +
    /// hash), the profile (phone/status), and SSO identities. A surviving
    /// name/email/phone would defeat right-to-erasure.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn erasure_clears_own_identity_pii() {
        let p = pool();
        let participants = ParticipantRepo::new(p.clone());

        let id = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name, avatar_url) \
             VALUES ($1, 'human', 'Jane Doe', 'https://cdn/jane.png')",
        )
        .bind(id.to_uuid())
        .execute(&p)
        .await
        .expect("participant");
        sqlx::query("INSERT INTO credentials (participant_id, email, password_hash) VALUES ($1,$2,'h')")
            .bind(id.to_uuid())
            .bind(format!("jane-{}@example.com", id.to_uuid()))
            .execute(&p)
            .await
            .expect("credentials");
        sqlx::query(
            "INSERT INTO participant_profiles (participant_id, phone, status_text) \
             VALUES ($1, '+1-555-0100', 'on vacation')",
        )
        .bind(id.to_uuid())
        .execute(&p)
        .await
        .expect("profile");
        sqlx::query(
            "INSERT INTO sso_identities (issuer, subject, participant_id, email) \
             VALUES ('https://idp', $1, $2, $3)",
        )
        .bind(format!("subj-{}", id.to_uuid()))
        .bind(id.to_uuid())
        .bind(format!("jane-{}@idp.com", id.to_uuid()))
        .execute(&p)
        .await
        .expect("sso");

        // Authored private content (drafts / OOO text / unsent scheduled message).
        let r = room(&p, id).await;
        sqlx::query(
            "INSERT INTO message_drafts (participant_id, room_id, blocks) VALUES ($1,$2,'[]'::jsonb)",
        )
        .bind(id.to_uuid())
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .expect("draft");
        sqlx::query("INSERT INTO out_of_office (participant_id, message) VALUES ($1,'away — call me')")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("ooo");
        sqlx::query(
            "INSERT INTO scheduled_messages (id, room_id, sender_id, blocks, scheduled_at) \
             VALUES ($1,$2,$3,'[]'::jsonb, now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(r.to_uuid())
        .bind(id.to_uuid())
        .execute(&p)
        .await
        .expect("scheduled");
        sqlx::query("INSERT INTO totp_secrets (participant_id, secret) VALUES ($1,'JBSWY3DPEHPK3PXP')")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("totp");
        sqlx::query(
            "INSERT INTO saved_searches (id, participant_id, workspace_id, name, query) \
             VALUES ($1,$2,$3,'mine','secret query')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(id.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(&p)
        .await
        .expect("saved_search");
        // Behavioural-history PII added in 第五版 (search queries/clicks + login
        // IP/device history). Both must be erased; search_click_events has no FK
        // and login_events' cascade never fires (the participant is tombstoned, not
        // hard-deleted), so both rely on the explicit DELETEs in delete_participant.
        sqlx::query(
            "INSERT INTO search_click_events (participant_id, workspace_id, query_text, result_id, result_rank) \
             VALUES ($1,$2,'secret search',$3,0)",
        )
        .bind(id.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .bind(uuid::Uuid::new_v4())
        .execute(&p)
        .await
        .expect("search_click");
        sqlx::query("INSERT INTO login_events (participant_id, ip, user_agent) VALUES ($1,'203.0.113.7','Firefox')")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("login_event");
        // Persistent cross-room AI user profile (0145): re-identifiable PII keyed
        // by participant. Its FK is ON DELETE CASCADE but erasure tombstones the
        // participant (UPDATE), so the cascade never fires — the explicit DELETE in
        // delete_participant is load-bearing. Reverting that DELETE makes the
        // participant_ai_profiles assertion below fail.
        sqlx::query(
            "INSERT INTO participant_ai_profiles (participant_id, workspace_id, topics, preferences, summary) \
             VALUES ($1,$2,'[\"secret-topic\"]'::jsonb,'{\"tone\":\"concise\"}'::jsonb,'a private profile')",
        )
        .bind(id.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(&p)
        .await
        .expect("ai_profile");

        assert!(participants.delete_participant(id).await.expect("erase"));

        let (name, avatar_null): (String, bool) =
            sqlx::query_as("SELECT display_name, avatar_url IS NULL FROM participants WHERE id = $1")
                .bind(id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("reload participant");
        assert_eq!(name, "[deleted]", "display_name must be tombstoned");
        assert!(avatar_null, "avatar_url must be cleared");

        for (table, col) in [
            ("credentials", "participant_id"),
            ("participant_profiles", "participant_id"),
            ("sso_identities", "participant_id"),
            ("message_drafts", "participant_id"),
            ("out_of_office", "participant_id"),
            ("scheduled_messages", "sender_id"),
            ("totp_secrets", "participant_id"),
            ("saved_searches", "participant_id"),
            ("search_click_events", "participant_id"),
            ("login_events", "participant_id"),
            ("participant_ai_profiles", "participant_id"),
        ] {
            let count: (i64,) =
                sqlx::query_as(&format!("SELECT count(*) FROM {table} WHERE {col} = $1"))
                    .bind(id.to_uuid())
                    .fetch_one(&p)
                    .await
                    .expect("count");
            assert_eq!(count.0, 0, "{table} data must be deleted on erasure");
        }
    }

    /// GDPR erasure must delete moderation reports / ban appeals AUTHORED BY the
    /// erased user (their free-text `reason` / `appeal_reason` is PII), while
    /// keeping rows authored by OTHERS — including reports ABOUT the erased user —
    /// as other users' governance records. None of the three tables cascades on a
    /// tombstone erasure (ban_appeals / message_reports have no FK; user_reports'
    /// `reporter_id` cascade never fires because the participant is tombstoned, not
    /// hard-deleted), so the explicit DELETEs in `delete_participant` are load-bearing.
    #[tokio::test]
    #[ignore = "requires running Postgres with migrations applied"]
    async fn erasure_clears_authored_moderation_reports() {
        let p = pool();
        let participants = ParticipantRepo::new(p.clone());
        let id = participant(&p).await; // the erased user
        let other = participant(&p).await; // a different actor / target

        let other_appellant = uuid::Uuid::new_v4();
        // ban_appeals: the user's own appeal (deleted) + another's (kept).
        for (appellant, reason) in [(id.to_uuid(), "my appeal"), (other_appellant, "their appeal")] {
            sqlx::query(
                "INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason) VALUES ($1,$2,$3,$4)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(uuid::Uuid::new_v4())
            .bind(appellant)
            .bind(reason)
            .execute(&p)
            .await
            .expect("ban_appeal");
        }
        // message_reports: the user's own report (deleted) + another's (kept).
        for reporter in [id.to_uuid(), other.to_uuid()] {
            sqlx::query(
                "INSERT INTO message_reports (id, workspace_id, message_id, reporter_id, reason) \
                 VALUES ($1,$2,$3,$4,'spam')",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
            .bind(uuid::Uuid::new_v4())
            .bind(reporter)
            .execute(&p)
            .await
            .expect("message_report");
        }
        // user_reports: a report BY the erased user (deleted) + one ABOUT them (kept).
        sqlx::query("INSERT INTO user_reports (reporter_id, reported_id, reason) VALUES ($1,$2,'rude')")
            .bind(id.to_uuid())
            .bind(other.to_uuid())
            .execute(&p)
            .await
            .expect("user_report by id");
        sqlx::query("INSERT INTO user_reports (reporter_id, reported_id, reason) VALUES ($1,$2,'rude')")
            .bind(other.to_uuid())
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("user_report about id");
        // export_jobs: the user's own export job (deleted) + another's (kept).
        for pid in [id.to_uuid(), other.to_uuid()] {
            sqlx::query("INSERT INTO export_jobs (participant_id) VALUES ($1)")
                .bind(pid)
                .execute(&p)
                .await
                .expect("export_job");
        }
        // user_blocks: a block BY the erased user (deleted) + one ABOUT them (kept).
        sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1,$2)")
            .bind(id.to_uuid())
            .bind(other.to_uuid())
            .execute(&p)
            .await
            .expect("block by id");
        sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1,$2)")
            .bind(other.to_uuid())
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("block about id");

        assert!(participants.delete_participant(id).await.expect("erase"));

        // The erased user's OWN authored rows are gone.
        for (table, col) in [
            ("ban_appeals", "appellant_id"),
            ("message_reports", "reporter_id"),
            ("user_reports", "reporter_id"),
            ("export_jobs", "participant_id"),
            ("user_blocks", "blocker_id"),
        ] {
            let c: (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table} WHERE {col} = $1"))
                .bind(id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("count own");
            assert_eq!(c.0, 0, "{table} authored by the erased user must be deleted");
        }
        // Rows authored by OTHERS survive — including reports/blocks ABOUT the erased user.
        for (table, col, val) in [
            ("ban_appeals", "appellant_id", other_appellant),
            ("message_reports", "reporter_id", other.to_uuid()),
            ("user_reports", "reported_id", id.to_uuid()),
            ("export_jobs", "participant_id", other.to_uuid()),
            ("user_blocks", "blocked_id", id.to_uuid()),
        ] {
            let c: (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table} WHERE {col} = $1"))
                .bind(val)
                .fetch_one(&p)
                .await
                .expect("count kept");
            assert_eq!(c.0, 1, "{table} authored by / about others must be kept");
        }
    }

    /// A deleted participant's Personal Access Token must STOP authenticating —
    /// the highest-severity erasure gap (a surviving credential = ongoing API
    /// access for a "deleted" account). Both halves are exercised: erasure deletes
    /// the token row, AND `PatRepo::verify` rejects a tombstoned owner even if a
    /// token row somehow survives.
    #[tokio::test]
    #[ignore = "requires running Postgres with migrations applied"]
    async fn erasure_revokes_personal_access_tokens() {
        let p = pool();
        let participants = ParticipantRepo::new(p.clone());
        let pats = crate::PatRepo::new(p.clone());

        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human','Tokened')")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("participant");
        let hash = format!("pat-hash-{}", id.to_uuid());
        pats.create(id, &hash, Some("ci"), &[], None).await.expect("create pat");

        // Before erasure the PAT authenticates its owner.
        assert_eq!(
            pats.verify(&hash).await.expect("verify ok"),
            Some(id),
            "a live PAT authenticates before erasure",
        );

        assert!(participants.delete_participant(id).await.expect("erase"));

        // After erasure the PAT no longer authenticates (row deleted).
        assert_eq!(
            pats.verify(&hash).await.expect("verify after erase"),
            None,
            "a deleted participant's PAT must not authenticate",
        );
        let remaining: (i64,) =
            sqlx::query_as("SELECT count(*) FROM pat_tokens WHERE participant_id = $1")
                .bind(id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("count pats");
        assert_eq!(remaining.0, 0, "pat_tokens must be erased");

        // Defence-in-depth: even a manually re-inserted token for a tombstoned
        // participant is rejected by the deleted_at guard in verify().
        sqlx::query(
            "INSERT INTO pat_tokens (id, participant_id, token_hash, name, scopes, created_at) \
             VALUES ($1,$2,$3,'rogue','{}', now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(id.to_uuid())
        .bind(&hash)
        .execute(&p)
        .await
        .expect("reinsert");
        assert_eq!(
            pats.verify(&hash).await.expect("verify rogue"),
            None,
            "verify() rejects a token whose owner is tombstoned, even if the row exists",
        );

        // Cleanup.
        sqlx::query("DELETE FROM pat_tokens WHERE participant_id = $1").bind(id.to_uuid()).execute(&p).await.ok();
    }

    /// GDPR erasure must remove the deleted participant's call-plane PII. A
    /// `call_transcripts` row is verbatim transcribed speech keyed by `speaker_id`
    /// with NO foreign key (0064), so it survives erasure unless deleted in
    /// `delete_participant` — and could then be read back via the call-transcript
    /// API for a "deleted" user. This guards the explicit DELETE: only the erased
    /// speaker's own lines go (a co-speaker's line in the SAME call is kept), and
    /// the user's `call_participants` membership row is removed. Reverting the
    /// DELETEs makes this fail (transcript / membership rows survive).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn erasure_deletes_call_transcripts_and_membership() {
        let p = pool();
        let participants = ParticipantRepo::new(p.clone());

        let speaker = participant(&p).await;
        let other = participant(&p).await;
        let r = room(&p, speaker).await;

        // A call session in that room, initiated by `speaker`.
        let call_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room_id, initiator, kind) \
             VALUES ($1, $2, $3, 'audio')",
        )
        .bind(call_id)
        .bind(r.to_uuid())
        .bind(speaker.to_uuid())
        .execute(&p)
        .await
        .expect("call_sessions");

        // Both took part (per-leg membership rows).
        for (pid, role) in [(speaker, "caller"), (other, "callee")] {
            sqlx::query(
                "INSERT INTO call_participants (call_id, participant_id, role) \
                 VALUES ($1, $2, $3)",
            )
            .bind(call_id)
            .bind(pid.to_uuid())
            .bind(role)
            .execute(&p)
            .await
            .expect("call_participants");
        }

        // A spoken line from each — the erased user's is PII to remove, the
        // co-speaker's must be preserved.
        for (pid, text) in
            [(speaker, "my private spoken secret"), (other, "co-speaker line to keep")]
        {
            sqlx::query(
                "INSERT INTO call_transcripts (id, call_id, speaker_id, text) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(call_id)
            .bind(pid.to_uuid())
            .bind(text)
            .execute(&p)
            .await
            .expect("call_transcripts");
        }

        assert!(participants.delete_participant(speaker).await.expect("erase"));

        // The erased speaker's verbatim transcript lines are gone.
        let mine: (i64,) =
            sqlx::query_as("SELECT count(*) FROM call_transcripts WHERE speaker_id = $1")
                .bind(speaker.to_uuid())
                .fetch_one(&p)
                .await
                .expect("count own transcripts");
        assert_eq!(mine.0, 0, "erased speaker's transcript PII must be deleted");

        // The co-speaker's line in the SAME call survives (erase OWN data only).
        let theirs: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM call_transcripts WHERE call_id = $1 AND speaker_id = $2",
        )
        .bind(call_id)
        .bind(other.to_uuid())
        .fetch_one(&p)
        .await
        .expect("count other transcripts");
        assert_eq!(theirs.0, 1, "a co-speaker's transcript line must be preserved");

        // The erased user's call membership row is gone (cascade never fires —
        // the participant is tombstoned, not hard-deleted).
        let membership: (i64,) =
            sqlx::query_as("SELECT count(*) FROM call_participants WHERE participant_id = $1")
                .bind(speaker.to_uuid())
                .fetch_one(&p)
                .await
                .expect("count membership");
        assert_eq!(membership.0, 0, "erased participant's call membership must be deleted");
    }

    /// `credentials.email` is `citext`, so a login must match regardless of case.
    /// A `citext = text` bind silently compares CASE-SENSITIVELY, which locked
    /// out every mixed-case email (register stored it verbatim; the login route
    /// lowercases the input → the lookup never matched). Guards the `$1::citext`
    /// cast in [`find_credentials_by_email`]. Regression: smoke_wave23.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn find_credentials_by_email_is_case_insensitive() {
        let p = pool();
        let repo = ParticipantRepo::new(p.clone());
        // Mixed-case local part + unique suffix so reruns don't collide on the
        // case-insensitive unique index.
        let suffix = ParticipantId::new();
        let stored = format!("MixedCase+{suffix}@Example.COM");
        // Register the email WRAPPED IN WHITESPACE so this also proves the
        // store-side trim (create_human), not just the lookup-side trim.
        let created = repo
            .create_human(super::NewHuman {
                email: format!("   {stored}\t"),
                display_name: "Case Test".into(),
                password_hash: "x".into(),
            })
            .await
            .expect("create_human");

        // Case variants (citext) AND whitespace-wrapped variants (the column is
        // case- but NOT whitespace-insensitive; create_human + the lookup both trim,
        // mirroring the login route's `.trim().to_lowercase()` — else a stored
        // `" a@x "` would never match a trimmed login → permanent lockout).
        for variant in [
            stored.clone(),
            stored.to_lowercase(),
            stored.to_uppercase(),
            format!("  {stored}  "),
            format!("\t{}\n", stored.to_lowercase()),
        ] {
            let found = repo
                .find_credentials_by_email(&variant)
                .await
                .expect("lookup")
                .unwrap_or_else(|| panic!("email lookup must be case/whitespace-insensitive (failed for {variant:?})"));
            assert_eq!(
                found.participant_id, created.id,
                "variant {variant:?} matched the wrong (or no) account",
            );
        }
    }
