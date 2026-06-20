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
        // Store the email TRIMMED so it matches the login lookup, which trims +
        // lowercases its input (routes.rs `auth_login`). `citext` already makes the
        // column case-insensitive, but NOT whitespace-insensitive — a stored
        // `" a@x.com"` would never match a trimmed `"a@x.com"` at login → permanent
        // lockout (same blast radius as the citext bug, different normalization axis).
        .bind(new.email.trim())
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
        // `credentials.email` is `citext` (case-insensitive) so e.g. `Foo@x.io`
        // and `foo@x.io` are one account. BUT sqlx binds `$1` as `text`, and a
        // `citext = text` comparison degrades to CASE-SENSITIVE (Postgres resolves
        // it as text equality, not citext). That silently broke login for every
        // mixed-case email: registration stored it verbatim, but the login route
        // lowercases the input, so `WHERE email = 'foo@x.io'` never matched the
        // stored `Foo@x.io` → permanent lockout. Casting `$1::citext` forces the
        // intended case-insensitive `citext = citext` comparison. (Regression:
        // smoke_wave23 non-member login with a mixed-case email.)
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            r#"SELECT participant_id, email, password_hash FROM credentials WHERE email = $1::citext"#,
        )
        // Trim to match how the email is normalized on the write path (see
        // `create_human`): the column is whitespace-SENSITIVE even as `citext`.
        .bind(email.trim())
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
        // Trim so a changed address stays consistent with the trimmed login lookup
        // (mirrors `create_human`; `citext` is case- but not whitespace-insensitive).
        .bind(new_email.trim())
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
            // Behavioural-history tables added in 第五版 (ROADMAP5 方向三/五). Both
            // carry re-identifiable PII keyed by participant and must be erased:
            //   - search_click_events: the user's search query_text + clicked
            //     results (no FK — it deliberately survives result deletion, so it
            //     is NOT cascade-cleaned and MUST be deleted explicitly here).
            //   - login_events: source IPs + user-agents. Its FK is ON DELETE
            //     CASCADE, but erasure TOMBSTONES the participant row (UPDATE) rather
            //     than hard-deleting it, so the cascade never fires — delete explicitly.
            "DELETE FROM search_click_events WHERE participant_id = $1",
            "DELETE FROM login_events WHERE participant_id = $1",
            // Call-plane PII (P3 voice/video). Both must be deleted explicitly:
            //   - call_transcripts (0064): verbatim transcribed speech keyed by
            //     speaker_id, with NO foreign key — the migration deliberately lets a
            //     transcript outlive the participant, so nothing cascades and the
            //     re-identifiable speech survives erasure unless deleted here. Scope
            //     to `speaker_id = $1` so only the erased user's own spoken lines go;
            //     other speakers' lines in the same call are kept (erase OWN data).
            //   - call_participants (0002): the user's per-leg membership rows. Its
            //     participant_id FK is ON DELETE CASCADE, but erasure TOMBSTONES the
            //     participant row (UPDATE) instead of hard-deleting it, so the cascade
            //     never fires — delete explicitly (same reasoning as login_events).
            // call_sessions is deliberately NOT touched: its only participant ref is
            // `initiator` (an opaque id, already de-identified once the participant
            // row is tombstoned, exactly like messages.sender_id), it holds no
            // free-text PII, and the session/recap is a shared call audit record whose
            // removal would rewrite other participants' history.
            "DELETE FROM call_transcripts WHERE speaker_id = $1",
            "DELETE FROM call_participants WHERE participant_id = $1",
            // Auth credentials / identity material. Leaving any of these behind is
            // worse than a PII leak — a surviving credential keeps AUTHENTICATING a
            // "deleted" account (refresh sessions are revoked above, but these are
            // separate long-lived credentials erasure previously missed): a PAT
            // grants ongoing API access, push tokens keep pushing to the user's
            // devices, recovery/reset/history material enables account recovery, and
            // mls_key_packages / scim_users are identity linkage.
            "DELETE FROM pat_tokens WHERE participant_id = $1",
            "DELETE FROM push_tokens WHERE participant_id = $1",
            "DELETE FROM recovery_codes WHERE participant_id = $1",
            "DELETE FROM password_reset_tokens WHERE participant_id = $1",
            "DELETE FROM password_history WHERE participant_id = $1",
            "DELETE FROM mls_key_packages WHERE participant_id = $1",
            "DELETE FROM scim_users WHERE participant_id = $1",
            // Remaining personal preferences / own-state PII (no cross-user
            // display depends on these, so deleting them breaks nothing for
            // others — they are purely the erased user's own settings/history).
            // Cross-user aggregates (reactions, poll_votes, read receipts on
            // OTHERS' messages) are deliberately NOT deleted: once the participant
            // row is tombstoned they are already de-identified, and removing them
            // would silently rewrite other users' visible counts.
            "DELETE FROM dnd_settings WHERE participant_id = $1",
            "DELETE FROM channel_mutes WHERE participant_id = $1",
            "DELETE FROM bookmarks WHERE participant_id = $1",
            "DELETE FROM bookmark_collections WHERE participant_id = $1",
            "DELETE FROM user_status WHERE participant_id = $1",
            "DELETE FROM thread_read_state WHERE participant_id = $1",
            "DELETE FROM ooo_auto_replies WHERE sender_id = $1",
            // Persistent cross-room AI user profile (持久跨房 AI 用户画像, 0145).
            // Re-identifiable PII derived from the participant's cross-room
            // messages (extracted topics / preferences / a free-text summary).
            // Its FK is ON DELETE CASCADE, but erasure TOMBSTONES the participant
            // row (UPDATE) instead of hard-deleting it, so the cascade never fires
            // — delete explicitly (project invariant: every participant-keyed PII
            // table must be wired into erasure; CASCADE is not enough because
            // erasure is a tombstone, not a hard delete).
            "DELETE FROM participant_ai_profiles WHERE participant_id = $1",
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
mod db_tests;
