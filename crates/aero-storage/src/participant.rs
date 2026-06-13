//! Participant + credential repositories. Implementation is delegated to the
//! storage agent (see Spec §3.3).

use aero_common::{Participant, ParticipantId, ParticipantKind};
use sqlx::PgPool;

#[derive(Clone)]
pub struct ParticipantRepo {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewHuman {
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
}

#[derive(Debug, Clone)]
pub struct CredentialRecord {
    pub participant_id: ParticipantId,
    pub email: String,
    pub password_hash: String,
}

impl ParticipantRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Inserts a new Human participant + credentials row in one transaction.
    /// Returns the created `Participant`. Email is unique.
    pub async fn create_human(&self, new: NewHuman) -> Result<Participant, sqlx::Error> {
        let id = ParticipantId::new();
        let mut tx = self.pool.begin().await?;
        let created_at = time::OffsetDateTime::now_utc();

        sqlx::query(
            r#"INSERT INTO participants (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, 'human', $2, NULL, NULL, $3)"#,
        )
        .bind(id.to_uuid())
        .bind(&new.display_name)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"INSERT INTO credentials (participant_id, email, password_hash, created_at)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(id.to_uuid())
        .bind(&new.email)
        .bind(&new.password_hash)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Participant {
            id,
            kind: ParticipantKind::Human,
            display_name: new.display_name,
            avatar_url: None,
            created_by: None,
            created_at,
        })
    }

    pub async fn find_credentials_by_email(
        &self,
        email: &str,
    ) -> Result<Option<CredentialRecord>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            r#"SELECT participant_id, email, password_hash FROM credentials WHERE email = $1"#,
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(pid, email, hash)| CredentialRecord {
            participant_id: ParticipantId::from_uuid(pid),
            email,
            password_hash: hash,
        }))
    }

    /// Look up credentials by participant id (for password-change verification).
    ///
    /// Returns `None` when the participant has no credentials row (e.g. bots/agents).
    pub async fn find_credentials_by_participant_id(
        &self,
        participant_id: ParticipantId,
    ) -> Result<Option<CredentialRecord>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            r#"SELECT participant_id, email, password_hash FROM credentials WHERE participant_id = $1"#,
        )
        .bind(participant_id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(pid, email, hash)| CredentialRecord {
            participant_id: ParticipantId::from_uuid(pid),
            email,
            password_hash: hash,
        }))
    }

    /// Replace the email address stored for a participant.
    ///
    /// Returns `true` if the row existed and was updated, `false` when no
    /// credentials row exists for that participant (bots/agents). Propagates a
    /// unique-constraint violation as-is so callers can map it to `409 Conflict`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update, including a
    /// `UniqueViolation` if `new_email` is already taken by another account.
    pub async fn update_email(
        &self,
        participant_id: ParticipantId,
        new_email: &str,
    ) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE credentials SET email = $1 WHERE participant_id = $2",
        )
        .bind(new_email)
        .bind(participant_id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    /// GDPR-compliant account deletion: soft-delete the participant and
    /// anonymise their message content in one transaction.
    ///
    /// Steps (all within the same DB transaction):
    /// 1. Mark `participants.deleted_at = NOW()` (keeps the row for FK integrity)
    ///    and tombstone its own PII: `display_name = '[deleted]'`, `avatar_url = NULL`.
    /// 2. Hard-delete the satellite PII tables (`credentials` — login email + hash,
    ///    `participant_profiles` — phone/status/pronouns, `sso_identities` — IdP
    ///    email/subject) and unsent authored content not in the message ledger
    ///    (`message_drafts`, `out_of_office` text, `scheduled_messages`); nothing
    ///    references them, so they are removed outright.
    /// 3. Overwrite every non-deleted message the participant sent with a
    ///    `[deleted]` placeholder, clear `searchable_text`, and null the pgvector
    ///    `embedding` (a semantic vector is re-identifiable) (GDPR Art. 17) —
    ///    EXCEPT messages under an active legal hold (Art. 17(3)(e)).
    /// 4. Revoke all active `auth_sessions` so existing tokens stop working.
    ///
    /// Returns `true` when the account existed and was freshly soft-deleted,
    /// `false` when the participant was not found or was already deleted
    /// (idempotent).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn delete_participant(
        &self,
        participant_id: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as::<_, (Option<time::OffsetDateTime>,)>(
            "SELECT deleted_at FROM participants WHERE id = $1",
        )
        .bind(participant_id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;

        // Not found or already deleted — nothing to do.
        match row {
            None | Some((Some(_),)) => {
                tx.commit().await?;
                return Ok(false);
            }
            Some((None,)) => {}
        }

        let now = time::OffsetDateTime::now_utc();

        // Anonymise the participant's own identity (GDPR right-to-erasure). The row
        // itself is kept (FK integrity for messages/rooms it is referenced by), but
        // its display_name is tombstoned and avatar_url cleared so no personal name
        // or photo survives the erasure.
        sqlx::query(
            "UPDATE participants SET deleted_at = $1, display_name = '[deleted]', avatar_url = NULL \
             WHERE id = $2",
        )
        .bind(now)
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;

        // Hard-delete the participant's satellite PII and unsent authored content —
        // nothing references these tables, so a plain delete fully removes them:
        //   identity PII: login email + hash (credentials), profile phone/status/
        //                 pronouns (participant_profiles), linked IdP emails/subjects
        //                 (sso_identities);
        //   authored private content not in the message ledger: unsent drafts
        //                 (message_drafts.blocks), the out-of-office autoreply text,
        //                 and not-yet-sent scheduled messages (scheduled_messages.blocks).
        //   security: the 2FA secret of the erased account (totp_secrets);
        //   personal data / settings: saved searches + watched keywords + message
        //                 templates (user-authored), the activity feed, recurring
        //                 messages, and per-channel/thread/workspace preferences.
        // (Deliberately kept: org_reports + revoked_tokens + workspace_deactivations
        //  are governance/audit records, and auth_sessions are revoked above, not
        //  deleted. `participants` itself is soft-deleted + tombstoned, not removed.)
        for stmt in [
            "DELETE FROM credentials WHERE participant_id = $1",
            "DELETE FROM participant_profiles WHERE participant_id = $1",
            "DELETE FROM sso_identities WHERE participant_id = $1",
            "DELETE FROM totp_secrets WHERE participant_id = $1",
            "DELETE FROM message_drafts WHERE participant_id = $1",
            "DELETE FROM out_of_office WHERE participant_id = $1",
            "DELETE FROM scheduled_messages WHERE sender_id = $1",
            "DELETE FROM recurring_messages WHERE sender_id = $1",
            "DELETE FROM saved_searches WHERE participant_id = $1",
            "DELETE FROM keyword_alerts WHERE participant_id = $1",
            "DELETE FROM message_templates WHERE participant_id = $1",
            "DELETE FROM activity_feed WHERE participant_id = $1",
            "DELETE FROM channel_favorites WHERE participant_id = $1",
            "DELETE FROM channel_notification_prefs WHERE participant_id = $1",
            "DELETE FROM channel_sections WHERE participant_id = $1",
            "DELETE FROM thread_mutes WHERE participant_id = $1",
            "DELETE FROM thread_notification_prefs WHERE participant_id = $1",
            "DELETE FROM thread_subscriptions WHERE participant_id = $1",
            "DELETE FROM workspace_mutes WHERE participant_id = $1",
            "DELETE FROM digest_subscriptions WHERE participant_id = $1",
            "DELETE FROM user_group_members WHERE participant_id = $1",
        ] {
            sqlx::query(stmt).bind(participant_id.to_uuid()).execute(&mut *tx).await?;
        }

        // Anonymise message content (GDPR right-to-erasure). Null the pgvector
        // `embedding` too: a semantic vector of the original text is re-identifiable
        // (a nearest-neighbour search reconstructs what was "erased"), so clearing
        // blocks/searchable_text without it leaves erasure incomplete. Mirrors the
        // soft-delete / retention-sweep paths (message.rs:101,154 / workspace.rs:826).
        //
        // EXEMPT messages under an active legal hold: GDPR Art. 17(3)(e) — erasure
        // does not apply to data that must be retained for legal claims. Mirrors the
        // retention sweep's exemption (workspace.rs:835). A hold covers a specific
        // room, or (room_id IS NULL) the whole workspace. NOTE: holds that release
        // *after* erasure leave these messages un-erased — completing erasure on
        // hold release is a deferred follow-up (would need an erasure queue).
        sqlx::query(
            r#"UPDATE messages m
               SET blocks          = '[{"type":"text","text":"[deleted]"}]'::jsonb,
                   searchable_text = '',
                   embedding       = NULL
               WHERE m.sender_id = $1
                 AND m.deleted_at IS NULL
                 AND NOT EXISTS (
                       SELECT 1 FROM rooms r
                        JOIN legal_holds lh
                          ON lh.active
                         AND (lh.room_id = r.id
                              OR (lh.room_id IS NULL AND lh.workspace_id = r.workspace_id))
                        WHERE r.id = m.room_id
                     )"#,
        )
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;

        // Revoke all active sessions so existing tokens are immediately invalid.
        sqlx::query(
            "UPDATE auth_sessions SET revoked_at = $1 WHERE participant_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;

        // Enqueue all owned blobs for background storage deletion.
        sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id)
              SELECT id FROM blobs WHERE owner_id = $1
              ON CONFLICT (blob_id) DO NOTHING",
        )
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(true)
    }

    /// Complete GDPR erasure for messages that [`delete_participant`] had to skip
    /// because they sat under a legal hold which has since released.
    ///
    /// Re-applies the erasure anonymisation to any message whose sender is already
    /// soft-deleted, that is no longer under an active legal hold, and that still
    /// carries identifiable content (text or embedding). Idempotent — already-erased
    /// rows are excluded by the content predicate, so it is safe to run on a timer.
    /// Returns the number of messages erased this pass.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn sweep_deferred_erasure(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE messages m
               SET blocks          = '[{"type":"text","text":"[deleted]"}]'::jsonb,
                   searchable_text = '',
                   embedding       = NULL
               FROM participants p
               WHERE m.sender_id = p.id
                 AND p.deleted_at IS NOT NULL
                 AND m.deleted_at IS NULL
                 AND (m.searchable_text <> '' OR m.embedding IS NOT NULL)
                 AND NOT EXISTS (
                       SELECT 1 FROM rooms r
                        JOIN legal_holds lh
                          ON lh.active
                         AND (lh.room_id = r.id
                              OR (lh.room_id IS NULL AND lh.workspace_id = r.workspace_id))
                        WHERE r.id = m.room_id
                     )"#,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Replace the password hash stored for a participant.
    ///
    /// Returns `true` if the row existed and was updated, `false` when no
    /// credentials row exists for that participant (bots/agents).
    pub async fn update_password_hash(
        &self,
        participant_id: ParticipantId,
        new_hash: &str,
    ) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            r#"UPDATE credentials SET password_hash = $1 WHERE participant_id = $2"#,
        )
        .bind(new_hash)
        .bind(participant_id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    /// Create a non-human participant (Bot or Agent). No credentials row.
    pub async fn create_bot(
        &self,
        display_name: &str,
        kind: ParticipantKind,
        created_by: Option<ParticipantId>,
        avatar_url: Option<&str>,
    ) -> Result<Participant, sqlx::Error> {
        let id = ParticipantId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let kind_s = match kind {
            ParticipantKind::Bot => "bot",
            ParticipantKind::Agent => "agent",
            ParticipantKind::Human => "human",
        };
        sqlx::query(
            r#"INSERT INTO participants (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(id.to_uuid())
        .bind(kind_s)
        .bind(display_name)
        .bind(avatar_url)
        .bind(created_by.map(|p| p.to_uuid()))
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(Participant {
            id,
            kind,
            display_name: display_name.to_owned(),
            avatar_url: avatar_url.map(str::to_owned),
            created_by,
            created_at,
        })
    }

    /// Substring search over display_name + credentials.email. Returns up to
    /// `limit` participants ordered by display_name. Excludes soft-removed rows.
    pub async fn search(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Participant>, sqlx::Error> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", q.replace('%', "\\%"));
        let limit = limit.clamp(1, 50);
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, Option<String>, Option<uuid::Uuid>, time::OffsetDateTime)>(
            r#"SELECT DISTINCT p.id, p.kind, p.display_name, p.avatar_url, p.created_by, p.created_at
               FROM participants p
               LEFT JOIN credentials c ON c.participant_id = p.id
               WHERE p.deleted_at IS NULL AND (p.display_name ILIKE $1 OR c.email ILIKE $1)
               ORDER BY p.display_name ASC
               LIMIT $2"#,
        )
        .bind(&pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, kind, name, avatar, creator, at)| {
                let kind = match kind.as_str() {
                    "human" => ParticipantKind::Human,
                    "agent" => ParticipantKind::Agent,
                    _ => ParticipantKind::Bot,
                };
                Participant {
                    id: ParticipantId::from_uuid(id),
                    kind,
                    display_name: name,
                    avatar_url: avatar,
                    created_by: creator.map(ParticipantId::from_uuid),
                    created_at: at,
                }
            })
            .collect())
    }

    pub async fn list_bots_in_room(
        &self,
        room: aero_common::RoomId,
    ) -> Result<Vec<Participant>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, Option<String>, Option<uuid::Uuid>, time::OffsetDateTime)>(
            r#"SELECT p.id, p.kind, p.display_name, p.avatar_url, p.created_by, p.created_at
               FROM participants p
               JOIN room_members m ON m.participant_id = p.id
               WHERE m.room_id = $1 AND p.kind IN ('bot','agent') AND p.deleted_at IS NULL"#,
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, kind, name, avatar, creator, at)| {
                let kind = match kind.as_str() {
                    "human" => ParticipantKind::Human,
                    "agent" => ParticipantKind::Agent,
                    _ => ParticipantKind::Bot,
                };
                Participant {
                    id: ParticipantId::from_uuid(id),
                    kind,
                    display_name: name,
                    avatar_url: avatar,
                    created_by: creator.map(ParticipantId::from_uuid),
                    created_at: at,
                }
            })
            .collect())
    }

    /// Patch display_name and/or avatar_url. Passing `None` for a field leaves
    /// it unchanged. Returns the updated row.
    pub async fn update_profile(
        &self,
        id: ParticipantId,
        display_name: Option<&str>,
        avatar_url: Option<Option<&str>>,
    ) -> Result<Option<Participant>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String, Option<String>, Option<uuid::Uuid>, time::OffsetDateTime)>(
            r#"UPDATE participants SET
                 display_name = COALESCE($2, display_name),
                 avatar_url   = CASE
                                  WHEN $3::boolean THEN $4
                                  ELSE avatar_url
                                END
               WHERE id = $1
            RETURNING id, kind, display_name, avatar_url, created_by, created_at"#,
        )
        .bind(id.to_uuid())
        .bind(display_name)
        .bind(avatar_url.is_some())
        .bind(avatar_url.flatten())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, kind, name, avatar, creator, at)| {
            let kind = match kind.as_str() {
                "human" => ParticipantKind::Human,
                "agent" => ParticipantKind::Agent,
                _ => ParticipantKind::Bot,
            };
            Participant {
                id: ParticipantId::from_uuid(id),
                kind,
                display_name: name,
                avatar_url: avatar,
                created_by: creator.map(ParticipantId::from_uuid),
                created_at: at,
            }
        }))
    }

    /// Set or clear the verified badge on a participant (migration 0116).
    ///
    /// `verified = true` sets `is_verified = TRUE` and records `verified_at = NOW()`.
    /// `verified = false` clears both columns. Idempotent in both directions.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn set_verified(
        &self,
        participant: ParticipantId,
        verified: bool,
    ) -> Result<(), sqlx::Error> {
        if verified {
            sqlx::query(
                "UPDATE participants SET is_verified = TRUE, verified_at = NOW() WHERE id = $1",
            )
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE participants SET is_verified = FALSE, verified_at = NULL WHERE id = $1",
            )
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn get(&self, id: ParticipantId) -> Result<Option<Participant>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String, Option<String>, Option<uuid::Uuid>, time::OffsetDateTime)>(
            r#"SELECT id, kind, display_name, avatar_url, created_by, created_at
               FROM participants WHERE id = $1 AND deleted_at IS NULL"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(id, kind, name, avatar, creator, created_at)| {
            let kind = match kind.as_str() {
                "human" => ParticipantKind::Human,
                "agent" => ParticipantKind::Agent,
                _ => ParticipantKind::Bot,
            };
            Participant {
                id: ParticipantId::from_uuid(id),
                kind,
                display_name: name,
                avatar_url: avatar,
                created_by: creator.map(ParticipantId::from_uuid),
                created_at,
            }
        }))
    }
}

#[cfg(test)]
mod db_tests {
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
}
