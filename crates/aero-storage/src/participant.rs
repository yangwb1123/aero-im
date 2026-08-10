//! Participant + credential repositories. Implementation is delegated to the
//! storage agent (see Spec §3.3).

use aero_common::{Error, Participant, ParticipantId, ParticipantKind, RoomId, WorkspaceId};
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

/// Expected account-erasure failures.
#[derive(Debug, thiserror::Error)]
pub enum ParticipantDeleteError {
    /// Workspace ownership must be explicitly transferred or demoted first.
    #[error("workspace ownership must be transferred before account deletion")]
    WorkspaceOwnerProtected,
    /// The account is the last effective owner of a channel.
    #[error("channel {0} ownership must be transferred before account deletion")]
    ChannelOwnerProtected(RoomId),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
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
            r"INSERT INTO participants (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, 'human', $2, NULL, NULL, $3)",
        )
        .bind(id.to_uuid())
        .bind(&new.display_name)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO credentials (participant_id, email, password_hash, created_at)
               VALUES ($1, $2, $3, $4)",
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
            r"SELECT participant_id, email, password_hash FROM credentials WHERE email = $1::citext",
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
            r"SELECT participant_id, email, password_hash FROM credentials WHERE participant_id = $1",
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
    /// Security-event audit: a successful change commits an `auth.email.changed`
    /// row in the SAME transaction (same-fate — the login identifier rewrite is
    /// the audited fact; a `UniqueViolation` still propagates through the tx so
    /// the handler's 409 mapping is unchanged). A zero-rows-affected call (no
    /// credentials row) audits nothing. Account-level event: workspace = the nil
    /// default tenant.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update, including a
    /// `UniqueViolation` if `new_email` is already taken by another account.
    pub async fn update_email(
        &self,
        participant_id: ParticipantId,
        new_email: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query("UPDATE credentials SET email = $1 WHERE participant_id = $2")
            // Trim so a changed address stays consistent with the trimmed login lookup
            // (mirrors `create_human`; `citext` is case- but not whitespace-insensitive).
            .bind(new_email.trim())
            .bind(participant_id.to_uuid())
            .execute(&mut *tx)
            .await?;
        let changed = rows.rows_affected() > 0;
        if changed {
            crate::AuditRepo::append_in_tx(
                &mut tx,
                aero_common::WorkspaceId::nil(),
                Some(participant_id),
                "auth.email.changed",
                Some(&participant_id.to_string()),
                serde_json::json!({}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// GDPR-compliant account deletion: soft-delete the participant and
    /// anonymise their message content in one transaction.
    ///
    /// Steps (all within the same DB transaction):
    /// 1. Mark `participants.deleted_at = NOW()` (keeps the row for FK integrity)
    ///    and tombstone its own PII: `display_name = '[deleted]'`, `avatar_url = NULL`.
    /// 2. Hard-delete the satellite PII tables (`credentials` — login email + hash,
    ///    `participant_profiles` — phone/status/pronouns, `sso_identities` — `IdP`
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
    ) -> Result<bool, ParticipantDeleteError> {
        let mut tx = self.pool.begin().await?;

        // Account deletion changes effective access in every tenant at once.
        // Migration 0198's direct-SQL guard takes this same low-frequency,
        // cross-tenant fence before PostgreSQL locks the participant target.
        // Call it explicitly before the narrower diagnostic locks below so the
        // later UPDATE statement only re-enters locks already held in canonical
        // order instead of trying to widen a partially-held workspace set.
        sqlx::query("SELECT aero_lock_all_channel_governance()")
            .execute(&mut *tx)
            .await?;

        // Retain the participant-scoped rows for the explicit error checks:
        // workspace rows -> channel rows -> external identities -> participant.
        // Identity migration and OIDC JIT both enter the concrete workspace
        // before taking their per-identity lifecycle lock, so erasure must not
        // acquire an identity lock while it can still wait on a workspace row.
        let workspace_ids = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace.id
                FROM workspaces workspace
                JOIN workspace_members membership
                  ON membership.workspace_id = workspace.id
                 AND membership.participant_id = $1
               ORDER BY workspace.id
               FOR UPDATE OF workspace",
        )
        .bind(participant_id.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        if !workspace_ids.is_empty() {
            sqlx::query_scalar::<_, uuid::Uuid>(
                r"SELECT room.id
                    FROM rooms room
                   WHERE room.workspace_id = ANY($1)
                     AND room.kind = 'channel'
                   ORDER BY room.workspace_id, room.id
                   FOR UPDATE",
            )
            .bind(&workspace_ids)
            .fetch_all(&mut *tx)
            .await?;
        }

        // External-identity operations use a per-(issuer, subject) advisory
        // lock. Acquire every current binding in database order after all
        // workspace/channel rows, but before the participant row below. This
        // matches migration/JIT's workspace -> identity -> participant order
        // while still preventing an identity -> participant inversion with a
        // concurrent repeat login.
        sqlx::query(
            r"SELECT aero_lock_external_identity(identity.issuer, identity.subject)
                FROM sso_identities identity
               WHERE identity.participant_id = $1
               ORDER BY identity.issuer, identity.subject",
        )
        .bind(participant_id.to_uuid())
        .fetch_all(&mut *tx)
        .await?;

        let row = sqlx::query_as::<_, (Option<time::OffsetDateTime>,)>(
            "SELECT deleted_at FROM participants WHERE id = $1 FOR UPDATE",
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

        if sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS (
                   SELECT 1
                     FROM workspace_members
                    WHERE participant_id = $1
                      AND role = 'owner'
               )",
        )
        .bind(participant_id.to_uuid())
        .fetch_one(&mut *tx)
        .await?
        {
            return Err(ParticipantDeleteError::WorkspaceOwnerProtected);
        }

        let stranded_channel = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT room.id
                FROM rooms room
                JOIN room_members owner_membership
                  ON owner_membership.room_id = room.id
                 AND owner_membership.participant_id = $1
                 AND owner_membership.role = 'owner'
               WHERE room.kind = 'channel'
                 AND aero_participant_has_effective_workspace_access(
                         room.workspace_id,
                         $1
                     )
                 AND NOT aero_channel_has_other_effective_owner(room.id, $1)
               ORDER BY room.id
               LIMIT 1",
        )
        .bind(participant_id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(room) = stranded_channel {
            return Err(ParticipantDeleteError::ChannelOwnerProtected(
                RoomId::from_uuid(room),
            ));
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

        // Action-item tasks are shared room work, not private idempotency
        // receipts. Migration 0226 permits exactly this metadata detach only
        // after the creator is tombstoned and the matching transaction-local
        // erasure actor is set. Detach first so deleting the 0224 receipt cannot
        // cascade-delete the preserved tasks.
        sqlx::query("SELECT set_config('aero.participant_erasure_actor', $1, true)")
            .bind(participant_id.to_uuid().to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r"UPDATE tasks
                  SET action_item_batch_key = NULL,
                      action_item_batch_index = NULL
                WHERE creator_id = $1
                  AND action_item_batch_key IS NOT NULL",
        )
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM task_action_item_batches WHERE participant_id = $1")
            .bind(participant_id.to_uuid())
            .execute(&mut *tx)
            .await?;

        // Preserve a deliberately retained pseudonymous deny record for every
        // erased external identity. Issuer/subject can still be personal data;
        // this security-retention record is distinct from ordinary profile PII.
        // The DELETE trigger introduced by migration 0231 takes a transaction
        // advisory lock per (issuer, subject); a concurrent JIT login therefore
        // waits until these tombstones commit and cannot recreate the account.
        // Keep this transition ahead of the generic PII-delete loop because the
        // original issuer/subject values are intentionally unavailable after it.
        let erased_identities = sqlx::query_as::<_, (String, String)>(
            r"DELETE FROM sso_identities
                WHERE participant_id = $1
            RETURNING issuer, subject",
        )
        .bind(participant_id.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        for (issuer, subject) in erased_identities {
            // Legacy installations could contain malformed empty/control keys.
            // The resolver now rejects those forever, so they need no reusable-
            // identity tombstone; skipping them keeps GDPR erasure available
            // while operators clean the NOT VALID legacy constraint backlog.
            if !crate::sso::valid_external_identity_component(&issuer)
                || !crate::sso::valid_external_identity_component(&subject)
            {
                tracing::warn!(
                    participant_id = %participant_id,
                    "erased malformed legacy SSO binding without a reusable identity tombstone"
                );
                continue;
            }
            sqlx::query(
                r"INSERT INTO sso_identity_tombstones
                      (issuer, subject, former_participant_id, reason)
                   VALUES ($1, $2, $3, 'account_erased')
                   ON CONFLICT (issuer, subject) DO NOTHING",
            )
            .bind(issuer)
            .bind(subject)
            .bind(participant_id.to_uuid())
            .execute(&mut *tx)
            .await?;
        }

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
            "DELETE FROM totp_secrets WHERE participant_id = $1",
            "DELETE FROM message_drafts WHERE participant_id = $1",
            // Client-send dedup hashes are derived from the sender's message
            // content and the participant row is tombstoned rather than deleted,
            // so ON DELETE CASCADE would never run.
            "DELETE FROM message_send_keys WHERE sender_id = $1",
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
            // Behavioural-history tables added in 第五版 (ROADMAP5 方向三/五).
            // All carry re-identifiable PII keyed by participant and must be erased:
            //   - search_impressions: full normalized queries plus ordered result
            //     snapshots. Its participant FK cannot fire because erasure is an
            //     UPDATE/tombstone, not a hard delete.
            //   - search_click_events: the user's search query_text + clicked
            //     results (no FK — it deliberately survives result deletion, so it
            //     is NOT cascade-cleaned and MUST be deleted explicitly here).
            //   - login_events: source IPs + user-agents. Its FK is ON DELETE
            //     CASCADE, but erasure TOMBSTONES the participant row (UPDATE) rather
            //     than hard-deleting it, so the cascade never fires — delete explicitly.
            "DELETE FROM search_click_events WHERE participant_id = $1",
            "DELETE FROM search_impressions WHERE participant_id = $1",
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
            "DELETE FROM participant_ai_profiles_scoped WHERE participant_id = $1",
            // Moderation reports / ban appeals AUTHORED BY the erased user. Each row
            // carries the user's own free-text `reason` / `appeal_reason` PII and is
            // keyed by them as the author, so scope to the authoring column: only the
            // erased user's OWN submissions go. Rows ABOUT them (message_reports /
            // user_reports on `reported_id`, appeals decided by other reviewers) are
            // kept as other users' moderation/governance records — the same
            // "erase OWN data, keep cross-user records" stance as reactions/poll_votes.
            //   - ban_appeals (0091): no FK → never cascades; delete explicitly.
            //   - message_reports (0093): no FK on reporter_id → never cascades.
            //   - user_reports (0112): reporter_id FK is ON DELETE CASCADE, but erasure
            //     TOMBSTONES the participant (UPDATE) so the cascade never fires —
            //     delete explicitly (same reasoning as login_events / call_participants).
            "DELETE FROM ban_appeals WHERE appellant_id = $1",
            "DELETE FROM message_reports WHERE reporter_id = $1",
            "DELETE FROM user_reports WHERE reporter_id = $1",
            // Data-export jobs the user requested (0070). `blob_id` points at a FULL
            // PII export of their account, and `error` can embed PII — yet the
            // participant_id FK is ON DELETE CASCADE, which (like login_events) never
            // fires because erasure tombstones the participant. Delete their own jobs
            // explicitly. (The archive blob itself is reclaimed by export retention /
            // blob GC; this removes the job row and its re-identifying linkage.)
            "DELETE FROM export_jobs WHERE participant_id = $1",
            // The user's OWN block list (0106) — a personal preference like
            // dnd_settings / channel_mutes above. Blocks are blocker-private, so
            // removing the erased user's entries changes nothing for anyone else;
            // rows where they are the BLOCKED party (others' lists) are kept.
            "DELETE FROM user_blocks WHERE blocker_id = $1",
        ] {
            sqlx::query(stmt)
                .bind(participant_id.to_uuid())
                .execute(&mut *tx)
                .await?;
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
        let anonymized_messages: Vec<(uuid::Uuid,)> = sqlx::query_as(
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
                     )
               RETURNING m.id"#,
        )
        .bind(participant_id.to_uuid())
        .fetch_all(&mut *tx)
        .await?;

        // Prior edit bodies are retained as audit rows, but they contain the same
        // authored content erased above. Anonymise those bodies for exactly the
        // non-held messages selected by the UPDATE; active legal holds therefore
        // preserve both the live row and its history. Interactive payloads are a
        // user-visible derivative of the retired blocks rather than evidence, so
        // remove them for the same message set.
        let anonymized_message_ids: Vec<uuid::Uuid> =
            anonymized_messages.into_iter().map(|(id,)| id).collect();
        if !anonymized_message_ids.is_empty() {
            sqlx::query(
                r#"UPDATE message_edits
                      SET blocks = '[{"type":"text","text":"[deleted]"}]'::jsonb
                    WHERE message_id = ANY($1)"#,
            )
            .bind(&anonymized_message_ids)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM block_interactions WHERE message_id = ANY($1)")
                .bind(&anonymized_message_ids)
                .execute(&mut *tx)
                .await?;
        }

        // Revoke and blacklist every active refresh session inside this same
        // erasure transaction. Updating auth_sessions alone would leave the JWT
        // refreshable because the refresh path consults revoked_tokens.
        sqlx::query(
            r"WITH revoked AS (
                   UPDATE auth_sessions
                      SET revoked_at = $1
                    WHERE participant_id = $2 AND revoked_at IS NULL
                RETURNING token_hash
               )
               INSERT INTO revoked_tokens (token_hash, participant_id)
               SELECT token_hash, $2 FROM revoked
               ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(now)
        .bind(participant_id.to_uuid())
        .execute(&mut *tx)
        .await?;

        // Enqueue all owned blobs for background storage deletion.
        sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id, force_delete)
              SELECT id, TRUE FROM blobs WHERE owner_id = $1
              ON CONFLICT (blob_id) DO UPDATE
                  SET force_delete = TRUE",
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
        let mut tx = self.pool.begin().await?;
        let erased: Vec<(uuid::Uuid,)> = sqlx::query_as(
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
                     )
               RETURNING m.id"#,
        )
        .fetch_all(&mut *tx)
        .await?;
        let message_ids: Vec<uuid::Uuid> = erased.into_iter().map(|(id,)| id).collect();
        if !message_ids.is_empty() {
            sqlx::query(
                r#"UPDATE message_edits
                      SET blocks = '[{"type":"text","text":"[deleted]"}]'::jsonb
                    WHERE message_id = ANY($1)"#,
            )
            .bind(&message_ids)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM block_interactions WHERE message_id = ANY($1)")
                .bind(&message_ids)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(u64::try_from(message_ids.len()).unwrap_or(u64::MAX))
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
        let rows =
            sqlx::query(r"UPDATE credentials SET password_hash = $1 WHERE participant_id = $2")
                .bind(new_hash)
                .bind(participant_id.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(rows.rows_affected() > 0)
    }

    /// Substring search over `display_name` + credentials.email. Returns up to
    /// `limit` participants ordered by `display_name`. Excludes soft-removed rows.
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<Participant>, sqlx::Error> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", q.replace('%', "\\%"));
        let limit = limit.clamp(1, 50);
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT DISTINCT p.id, p.kind, p.display_name, p.avatar_url, p.created_by, p.created_at
               FROM participants p
               LEFT JOIN credentials c ON c.participant_id = p.id
               WHERE p.deleted_at IS NULL AND (p.display_name ILIKE $1 OR c.email ILIKE $1)
               ORDER BY p.display_name ASC
               LIMIT $2",
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
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT p.id, p.kind, p.display_name, p.avatar_url, p.created_by, p.created_at
               FROM participants p
               JOIN room_members m ON m.participant_id = p.id
               WHERE m.room_id = $1 AND p.kind IN ('bot','agent') AND p.deleted_at IS NULL",
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

    /// Patch `display_name` and/or `avatar_url`. Passing `None` for a field leaves
    /// it unchanged. Returns the updated row.
    pub async fn update_profile(
        &self,
        id: ParticipantId,
        display_name: Option<&str>,
        avatar_url: Option<Option<&str>>,
    ) -> Result<Option<Participant>, sqlx::Error> {
        let row = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
            ),
        >(
            r"UPDATE participants SET
                 display_name = COALESCE($2, display_name),
                 avatar_url   = CASE
                                  WHEN $3::boolean THEN $4
                                  ELSE avatar_url
                                END
               WHERE id = $1
            RETURNING id, kind, display_name, avatar_url, created_by, created_at",
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

    /// Set or clear the verified badge while `actor` remains an effective
    /// administrator of the platform administration workspace.
    ///
    /// `verified = true` sets `is_verified = TRUE` and records `verified_at = NOW()`.
    /// `verified = false` clears both columns. Idempotent in both directions.
    ///
    /// The authorization fence, active-target lock, badge update, and audit
    /// append commit in one transaction.
    ///
    /// # Errors
    /// Returns [`Error::Forbidden`] unless `actor` remains an effective
    /// Owner/Admin, [`Error::NotFound`] for a missing/deleted target, and
    /// propagates storage errors.
    pub async fn set_verified_authorized(
        &self,
        platform_workspace: WorkspaceId,
        actor: ParticipantId,
        participant: ParticipantId,
        verified: bool,
    ) -> Result<Option<time::OffsetDateTime>, Error> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, platform_workspace, actor)
            .await?;
        let active = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NULL
               FROM participants
              WHERE id = $1
              FOR UPDATE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !active {
            return Err(Error::NotFound("active participant".into()));
        }

        let verified_at = sqlx::query_scalar::<_, Option<time::OffsetDateTime>>(
            "UPDATE participants
                SET is_verified = $2,
                    verified_at = CASE
                        WHEN $2 THEN COALESCE(verified_at, clock_timestamp())
                        ELSE NULL
                    END
              WHERE id = $1
          RETURNING verified_at",
        )
        .bind(participant.to_uuid())
        .bind(verified)
        .fetch_one(&mut *tx)
        .await?;
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            platform_workspace,
            Some(actor),
            "participant.verified",
            Some(&participant.to_string()),
            serde_json::json!({ "verified": verified }),
        )
        .await?;
        tx.commit().await?;
        Ok(verified_at)
    }

    pub async fn get(&self, id: ParticipantId) -> Result<Option<Participant>, sqlx::Error> {
        let row = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT id, kind, display_name, avatar_url, created_by, created_at
               FROM participants WHERE id = $1 AND deleted_at IS NULL",
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
#[path = "participant/credential_tests.rs"]
mod credential_tests;
#[cfg(test)]
mod db_tests;
#[cfg(test)]
#[path = "participant/erasure_moderation_tests.rs"]
mod erasure_moderation_tests;
#[cfg(test)]
#[path = "participant/ownership_tests.rs"]
mod ownership_tests;
#[cfg(test)]
#[path = "participant/verified_tests.rs"]
mod verified_tests;
