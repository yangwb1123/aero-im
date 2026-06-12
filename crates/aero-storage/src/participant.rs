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
    /// 1. Mark `participants.deleted_at = NOW()` — keeps the row for FK integrity.
    /// 2. Overwrite every non-deleted message the participant sent with a
    ///    `[deleted]` placeholder and clear `searchable_text` (GDPR Art. 17).
    /// 3. Revoke all active `auth_sessions` so existing tokens stop working.
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

        sqlx::query("UPDATE participants SET deleted_at = $1 WHERE id = $2")
            .bind(now)
            .bind(participant_id.to_uuid())
            .execute(&mut *tx)
            .await?;

        // Anonymise message content (GDPR right-to-erasure).
        sqlx::query(
            r#"UPDATE messages
               SET blocks          = '[{"type":"text","text":"[deleted]"}]'::jsonb,
                   searchable_text = ''
               WHERE sender_id = $1 AND deleted_at IS NULL"#,
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
