//! Blob repository — metadata for attachments stored in S3/MinIO.
//!
//! The bytes themselves live in object storage; this table tracks the metadata
//! and gates access (only owner + room members can fetch the binary payload).

use aero_common::{Blob, BlobId, Block, FileKind, ParticipantId, WorkspaceId};
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Clone)]
pub struct BlobRepo {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewBlob {
    pub owner_id: ParticipantId,
    pub kind: FileKind,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub storage_key: String,
}

/// Immutable placement metadata used to route blob bytes.
///
/// `storage_region = None` is reserved for rows created before region-aware
/// placement and is interpreted as the default backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStorageScope {
    pub workspace_id: Option<WorkspaceId>,
    pub storage_region: Option<String>,
}

impl BlobRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reserve blob metadata before writing bytes to the backing store.
    ///
    /// This legacy helper creates an unscoped (`workspace_id = NULL`) object.
    /// Such objects are intentionally ineligible for new message attachments;
    /// tenant content must use [`Self::reserve_in_scope`] with a workspace id.
    ///
    /// Reserved rows deliberately have `finalized_at = NULL`; all normal read,
    /// download, export, and dedup queries ignore them. The caller must write the
    /// object and then call [`Self::finalize`]. A crashed upload is therefore
    /// invisible and can be reclaimed by the stale-reservation GC path instead
    /// of becoming a permanently broken dedup hit.
    pub async fn reserve(&self, new: NewBlob) -> Result<Blob, sqlx::Error> {
        self.reserve_in_scope(new, None, None).await
    }

    /// Reserve metadata with the authenticated workspace and selected backend.
    ///
    /// These placement fields are immutable in the database. Callers must
    /// snapshot the workspace's current region here rather than consulting the
    /// mutable workspace setting during later reads or deletion.
    pub async fn reserve_in_scope(
        &self,
        new: NewBlob,
        workspace_id: Option<WorkspaceId>,
        storage_region: Option<&str>,
    ) -> Result<Blob, sqlx::Error> {
        let id = BlobId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let kind_s = match new.kind {
            FileKind::Image => "image",
            FileKind::Video => "video",
            FileKind::Audio => "audio",
            FileKind::Document => "document",
            FileKind::Other => "other",
        };
        let size_i = i64::try_from(new.size).unwrap_or(i64::MAX);
        sqlx::query(
            r#"INSERT INTO blobs
                 (id, owner_id, workspace_id, storage_region, kind, name, mime,
                  size, sha256, storage_key, created_at, finalized_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NULL)"#,
        )
        .bind(id.to_uuid())
        .bind(new.owner_id.to_uuid())
        .bind(workspace_id.map(|id| id.to_uuid()))
        .bind(storage_region)
        .bind(kind_s)
        .bind(&new.name)
        .bind(&new.mime)
        .bind(size_i)
        .bind(new.sha256.as_deref())
        .bind(&new.storage_key)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(Blob {
            id,
            owner_id: new.owner_id,
            workspace_id,
            storage_region: storage_region.map(str::to_owned),
            kind: new.kind,
            name: new.name,
            mime: new.mime,
            size: new.size,
            sha256: new.sha256,
            storage_key: new.storage_key,
            created_at,
            finalized_at: None,
        })
    }

    /// Publish a reservation after the backing object was written successfully.
    ///
    /// Returns `None` if the reservation no longer exists or was already
    /// finalized. The store's canonical key is persisted for operations and
    /// exports even though current backends can derive it from the blob id.
    pub async fn finalize(
        &self,
        id: BlobId,
        storage_key: &str,
    ) -> Result<Option<Blob>, sqlx::Error> {
        let row = sqlx::query_as::<_, BlobRow>(
            r#"UPDATE blobs AS b
                  SET storage_key = $2,
                      finalized_at = now()
                WHERE id = $1
                  AND finalized_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                  )
            RETURNING id, owner_id, workspace_id, storage_region, kind, name,
                      mime, size, sha256, storage_key, created_at, finalized_at"#,
        )
        .bind(id.to_uuid())
        .bind(storage_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Blob::from))
    }

    /// Remove an unpublished reservation. Finalized metadata is never removed
    /// through this cleanup seam.
    pub async fn discard_reservation(&self, id: BlobId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM blobs WHERE id = $1 AND finalized_at IS NULL")
            .bind(id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, id: BlobId) -> Result<Option<Blob>, sqlx::Error> {
        let row = sqlx::query_as::<_, BlobRow>(
            r#"SELECT id, owner_id, workspace_id, storage_region, kind, name,
                      mime, size, sha256, storage_key, created_at, finalized_at
               FROM blobs b
               WHERE id = $1
                 AND finalized_at IS NOT NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                 )"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Blob::from))
    }

    /// Read raw placement metadata for byte routing.
    ///
    /// Unlike [`Self::get`], this intentionally includes unfinished and
    /// GC-queued rows: abort and delete-then-ack must still reach the backend
    /// selected when the reservation was created.
    pub async fn storage_scope(&self, id: BlobId) -> Result<Option<BlobStorageScope>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<uuid::Uuid>, Option<String>)>(
            r"SELECT workspace_id, storage_region FROM blobs WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(workspace_id, storage_region)| BlobStorageScope {
            workspace_id: workspace_id.map(WorkspaceId::from_uuid),
            storage_region,
        }))
    }

    /// Authorization check for blob download (IDOR guard).
    ///
    /// A blob has no direct room column; it is linked to rooms only by being
    /// referenced from a `File`/`Voice` block (`{"blob_id": "<ulid>"}`) inside a
    /// message's `blocks` JSONB. A participant may access a blob when either:
    ///
    /// * they uploaded it (`blobs.owner_id`) and still have effective access to
    ///   its immutable workspace scope (legacy unscoped blobs remain personal), or
    /// * it is referenced by a message in a room they belong to, or
    /// * either custom-emoji store exposes it to their effectively accessible
    ///   workspace.
    ///
    /// The containment predicate (`blocks @> '[{"blob_id": ...}]'`) is served by
    /// the existing `messages_blocks_gin` GIN index. Read-only; no row is
    /// returned, only the boolean verdict.
    pub async fn is_accessible_by(
        &self,
        id: BlobId,
        viewer: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        // The blob id is stored in JSONB as its ULID string (BlobId is
        // `#[serde(transparent)]` over Ulid), so match on the Display form.
        let blob_ref = blob_ref_predicate(id);
        let row = sqlx::query_as::<_, (bool,)>(
            r"SELECT EXISTS (
                   SELECT 1
                     FROM blobs b
                    WHERE b.id = $1
                      AND b.finalized_at IS NOT NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                      )
                      AND (
                          (
                              b.owner_id = $2
                              AND (
                                  b.workspace_id IS NULL
                                  OR EXISTS (
                                      SELECT 1
                                        FROM workspaces owner_workspace
                                        JOIN workspace_members owner_membership
                                          ON owner_membership.workspace_id = owner_workspace.id
                                         AND owner_membership.participant_id = $2
                                        JOIN participants owner_viewer
                                          ON owner_viewer.id = $2
                                         AND owner_viewer.deleted_at IS NULL
                                        LEFT JOIN workspace_deactivations owner_deactivated
                                          ON owner_deactivated.workspace_id = owner_workspace.id
                                         AND owner_deactivated.participant_id = $2
                                        LEFT JOIN totp_secrets owner_totp
                                          ON owner_totp.participant_id = $2
                                       WHERE owner_workspace.id = b.workspace_id
                                         AND owner_deactivated.participant_id IS NULL
                                         AND (
                                             owner_viewer.kind <> 'human'
                                             OR NOT owner_workspace.require_2fa
                                             OR COALESCE(owner_totp.activated, false)
                                         )
                                  )
                              )
                          )
                          OR EXISTS (
                              SELECT 1
                                FROM messages m
                                JOIN rooms r ON r.id = m.room_id
                                JOIN workspaces w ON w.id = r.workspace_id
                                JOIN room_members rm
                                  ON rm.room_id = m.room_id AND rm.participant_id = $2
                                JOIN workspace_members wm
                                  ON wm.workspace_id = r.workspace_id AND wm.participant_id = $2
                                JOIN participants viewer
                                  ON viewer.id = $2 AND viewer.deleted_at IS NULL
                                LEFT JOIN workspace_deactivations deactivated
                                  ON deactivated.workspace_id = r.workspace_id
                                 AND deactivated.participant_id = $2
                               LEFT JOIN totp_secrets totp ON totp.participant_id = $2
                               WHERE deactivated.participant_id IS NULL
                                 AND (
                                     viewer.kind <> 'human'
                                     OR NOT w.require_2fa
                                     OR COALESCE(totp.activated, false)
                                 )
                                 AND b.workspace_id = r.workspace_id
                                 AND m.deleted_at IS NULL
                                 AND (m.expires_at IS NULL OR m.expires_at > now())
                                 AND m.blocks @> $3
                          )
                          OR EXISTS (
                              SELECT 1
                                FROM workspaces emoji_workspace
                                JOIN workspace_members emoji_membership
                                  ON emoji_membership.workspace_id = emoji_workspace.id
                                 AND emoji_membership.participant_id = $2
                                JOIN participants emoji_viewer
                                  ON emoji_viewer.id = $2
                                 AND emoji_viewer.deleted_at IS NULL
                                LEFT JOIN workspace_deactivations emoji_deactivated
                                  ON emoji_deactivated.workspace_id = emoji_workspace.id
                                 AND emoji_deactivated.participant_id = $2
                                LEFT JOIN totp_secrets emoji_totp
                                  ON emoji_totp.participant_id = $2
                               WHERE emoji_workspace.id = b.workspace_id
                                 AND emoji_deactivated.participant_id IS NULL
                                 AND (
                                     emoji_viewer.kind <> 'human'
                                     OR NOT emoji_workspace.require_2fa
                                     OR COALESCE(emoji_totp.activated, false)
                                 )
                                 AND (
                                     EXISTS (
                                         SELECT 1
                                           FROM custom_emoji emoji
                                          WHERE emoji.blob_id = b.id
                                            AND emoji.workspace_id = emoji_workspace.id
                                     )
                                     OR EXISTS (
                                         SELECT 1
                                           FROM workspace_emoji emoji
                                          WHERE emoji.blob_id = b.id
                                            AND emoji.workspace_id = emoji_workspace.id
                                     )
                                 )
                          )
                      )
               ) AS ok",
        )
        .bind(id.to_uuid())
        .bind(viewer.to_uuid())
        .bind(blob_ref)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Whether any currently visible message or custom emoji still references
    /// this blob.
    ///
    /// The GC worker rechecks this immediately before deleting bytes. It closes
    /// the ordinary enqueue→drain race where another live message or either
    /// historical workspace-emoji store may have acquired the object after the
    /// original reference was removed. Forced GDPR/expiry deletion deliberately
    /// bypasses this check; both emoji foreign keys cascade in that case.
    pub async fn has_live_references(&self, id: BlobId) -> Result<bool, sqlx::Error> {
        let row: (bool,) = sqlx::query_as(
            r"SELECT
                 EXISTS (
                     SELECT 1
                       FROM messages
                      WHERE deleted_at IS NULL
                        AND (expires_at IS NULL OR expires_at > now())
                        AND blocks @> $1
                 )
                 OR EXISTS (
                     SELECT 1 FROM custom_emoji WHERE blob_id = $2
                 )
                 OR EXISTS (
                     SELECT 1 FROM workspace_emoji WHERE blob_id = $2
                 )
                 OR EXISTS (
                     SELECT 1 FROM integration_blob_ledger WHERE blob_id = $2
                 )",
        )
        .bind(blob_ref_predicate(id))
        .bind(id.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Find an existing blob owned by `owner` with the exact SHA-256 content
    /// hash. Used for deduplication in `blob_upload`: if the caller re-uploads
    /// identical bytes, we return the existing blob id and skip the store write.
    ///
    /// Owner-scoped intentionally: sharing across owners would allow cross-user
    /// timing-channel hash detection (GDPR / privacy concern).
    pub async fn find_by_owner_sha256(
        &self,
        owner: ParticipantId,
        sha256: &str,
    ) -> Result<Option<Blob>, sqlx::Error> {
        self.find_by_owner_sha256_in_scope(owner, sha256, None, "default")
            .await
    }

    /// Find a finalized digest match inside one immutable residency boundary.
    ///
    /// Workspace and region are both part of the identity. Reusing an object
    /// across either boundary would silently defeat an administrator's data
    /// residency choice.
    pub async fn find_by_owner_sha256_in_scope(
        &self,
        owner: ParticipantId,
        sha256: &str,
        workspace_id: Option<WorkspaceId>,
        storage_region: &str,
    ) -> Result<Option<Blob>, sqlx::Error> {
        let row = sqlx::query_as::<_, BlobRow>(
            r"SELECT id, owner_id, workspace_id, storage_region, kind, name,
                      mime, size, sha256, storage_key, created_at, finalized_at
               FROM blobs b
               WHERE owner_id = $1
                 AND sha256 = $2
                 AND workspace_id IS NOT DISTINCT FROM $3
                 AND COALESCE(storage_region, 'default') = $4
                 AND finalized_at IS NOT NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                 )
               ORDER BY created_at DESC
               LIMIT 1",
        )
        .bind(owner.to_uuid())
        .bind(sha256)
        .bind(workspace_id.map(|id| id.to_uuid()))
        .bind(storage_region)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Blob::from))
    }

    /// All blobs uploaded by `owner`, newest first. Capped at `limit` rows.
    /// Used for personal GDPR data export (`GET /api/me/export`).
    pub async fn list_by_owner(
        &self,
        owner: ParticipantId,
        limit: i64,
    ) -> Result<Vec<Blob>, sqlx::Error> {
        // Defense in depth: bound the personal-export listing so a raw caller limit
        // can't stream an unbounded set (1000 is generous for the export page).
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query_as::<_, BlobRow>(
            r"SELECT id, owner_id, workspace_id, storage_region, kind, name,
                      mime, size, sha256, storage_key, created_at, finalized_at
               FROM blobs b
               WHERE owner_id = $1
                 AND finalized_at IS NOT NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                 )
               ORDER BY created_at DESC, id DESC
               LIMIT $2",
        )
        .bind(owner.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Blob::from).collect())
    }

    /// Lock and authorize all attachment IDs a message mutation wants to
    /// reference.
    ///
    /// The sender may attach their own finalized blob or reuse one that is
    /// already visible in a room they currently belong to, but the blob's
    /// immutable workspace scope must exactly match the target room's workspace.
    /// Legacy `workspace_id IS NULL` objects remain readable through
    /// [`Self::is_accessible_by`] for migration/export compatibility, but can
    /// never be introduced into a new message. Queued objects and unfinished
    /// upload reservations are rejected. The two-statement `FOR SHARE` protocol
    /// is load-bearing: acquire lifecycle locks first, then evaluate GC/ledger
    /// predicates in a fresh READ COMMITTED snapshot.
    pub(crate) async fn lock_message_attachments_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        blocks: &[Block],
        viewer: ParticipantId,
        target_room: aero_common::RoomId,
    ) -> Result<bool, sqlx::Error> {
        Self::lock_message_attachments_for_installation_in_tx(tx, blocks, viewer, target_room, None)
            .await
    }

    /// Integration variant of [`Self::lock_message_attachments_in_tx`]. An
    /// exact durable installation ledger remains valid across bot rotation;
    /// every ordinary workspace, room, finalization, and GC fence still applies.
    pub(crate) async fn lock_integration_attachments_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        blocks: &[Block],
        viewer: ParticipantId,
        target_room: aero_common::RoomId,
        installation_id: uuid::Uuid,
    ) -> Result<bool, sqlx::Error> {
        Self::lock_message_attachments_for_installation_in_tx(
            tx,
            blocks,
            viewer,
            target_room,
            Some(installation_id),
        )
        .await
    }

    async fn lock_message_attachments_for_installation_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        blocks: &[Block],
        viewer: ParticipantId,
        target_room: aero_common::RoomId,
        installation_id: Option<uuid::Uuid>,
    ) -> Result<bool, sqlx::Error> {
        let mut unique = std::collections::HashSet::new();
        for block in blocks {
            match block {
                Block::File { blob_id, .. } | Block::Voice { blob_id, .. } => {
                    unique.insert(*blob_id);
                }
                _ => {}
            }
        }
        if unique.is_empty() {
            return Ok(true);
        }

        let mut ids = Vec::with_capacity(unique.len());
        let mut refs = Vec::with_capacity(unique.len());
        for id in unique {
            ids.push(id.to_uuid());
            refs.push(id.to_string());
        }

        let locked = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT blob.id
                FROM blobs blob
               WHERE blob.id = ANY($1)
               ORDER BY blob.id
               FOR SHARE",
        )
        .bind(&ids)
        .fetch_all(&mut **tx)
        .await?;
        if locked.len() != ids.len() {
            return Ok(false);
        }

        let authorized = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"WITH requested(blob_id, blob_ref) AS (
                   SELECT * FROM unnest($1::uuid[], $2::text[])
               ),
               target AS (
                   SELECT room.workspace_id
                     FROM rooms room
                     JOIN workspaces workspace ON workspace.id = room.workspace_id
                     JOIN room_members target_room_member
                       ON target_room_member.room_id = room.id
                      AND target_room_member.participant_id = $3
                     JOIN workspace_members target_workspace_member
                       ON target_workspace_member.workspace_id = room.workspace_id
                      AND target_workspace_member.participant_id = $3
                     JOIN participants target_viewer
                       ON target_viewer.id = $3
                      AND target_viewer.deleted_at IS NULL
                     LEFT JOIN workspace_deactivations target_deactivated
                       ON target_deactivated.workspace_id = room.workspace_id
                      AND target_deactivated.participant_id = $3
                     LEFT JOIN totp_secrets target_totp
                       ON target_totp.participant_id = $3
                    WHERE room.id = $4
                      AND target_deactivated.participant_id IS NULL
                      AND (
                          target_viewer.kind <> 'human'
                          OR NOT workspace.require_2fa
                          OR COALESCE(target_totp.activated, false)
                      )
               )
               SELECT b.id
                 FROM requested r
                 JOIN blobs b ON b.id = r.blob_id
                 CROSS JOIN target
                WHERE b.finalized_at IS NOT NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM blob_gc_queue q WHERE q.blob_id = b.id
                  )
                  AND b.workspace_id = target.workspace_id
                  AND (
                      b.owner_id = $3
                      OR EXISTS (
                          SELECT 1
                            FROM messages m
                            JOIN rooms room ON room.id = m.room_id
                            JOIN workspaces w ON w.id = room.workspace_id
                            JOIN room_members rm
                              ON rm.room_id = m.room_id AND rm.participant_id = $3
                            JOIN workspace_members wm
                              ON wm.workspace_id = room.workspace_id AND wm.participant_id = $3
                            JOIN participants viewer
                              ON viewer.id = $3 AND viewer.deleted_at IS NULL
                            LEFT JOIN workspace_deactivations deactivated
                              ON deactivated.workspace_id = room.workspace_id
                             AND deactivated.participant_id = $3
                            LEFT JOIN totp_secrets totp ON totp.participant_id = $3
                           WHERE deactivated.participant_id IS NULL
                             AND (
                                 viewer.kind <> 'human'
                                 OR NOT w.require_2fa
                                 OR COALESCE(totp.activated, false)
                             )
                             AND m.deleted_at IS NULL
                             AND (m.expires_at IS NULL OR m.expires_at > now())
                             AND m.blocks @> jsonb_build_array(
                                 jsonb_build_object('blob_id', r.blob_ref)
                             )
                      )
                      OR EXISTS (
                          SELECT 1 FROM integration_blob_ledger ledger
                           WHERE $5::uuid IS NOT NULL
                             AND ledger.installation_id = $5
                             AND ledger.blob_id = b.id
                      )
                  )
               ORDER BY b.id",
        )
        .bind(&ids)
        .bind(&refs)
        .bind(viewer.to_uuid())
        .bind(target_room.to_uuid())
        .bind(installation_id)
        .fetch_all(&mut **tx)
        .await?;

        Ok(authorized.len() == ids.len())
    }
}

#[derive(sqlx::FromRow)]
struct BlobRow {
    id: uuid::Uuid,
    owner_id: uuid::Uuid,
    workspace_id: Option<uuid::Uuid>,
    storage_region: Option<String>,
    kind: String,
    name: String,
    mime: String,
    size: i64,
    sha256: Option<String>,
    storage_key: String,
    created_at: time::OffsetDateTime,
    finalized_at: Option<time::OffsetDateTime>,
}

/// JSONB containment predicate matching any message `blocks` array that
/// references `id`. Used as the right-hand side of the `blocks @> $3` filter in
/// [`BlobRepo::is_accessible_by`]. Factored out so the linkage shape can be
/// unit-tested against real serialized blocks without a database.
fn blob_ref_predicate(id: BlobId) -> serde_json::Value {
    serde_json::json!([{ "blob_id": id.to_string() }])
}

impl From<BlobRow> for Blob {
    fn from(r: BlobRow) -> Self {
        let kind = match r.kind.as_str() {
            "image" => FileKind::Image,
            "video" => FileKind::Video,
            "audio" => FileKind::Audio,
            "document" => FileKind::Document,
            _ => FileKind::Other,
        };
        Self {
            id: BlobId::from_uuid(r.id),
            owner_id: ParticipantId::from_uuid(r.owner_id),
            workspace_id: r.workspace_id.map(WorkspaceId::from_uuid),
            storage_region: r.storage_region,
            kind,
            name: r.name,
            mime: r.mime,
            size: u64::try_from(r.size).unwrap_or(0),
            sha256: r.sha256,
            storage_key: r.storage_key,
            created_at: r.created_at,
            finalized_at: r.finalized_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Block, FileKind};
    use serde_json::Value;

    /// Faithful re-implementation of the Postgres `@>` (jsonb-contains) operator,
    /// enough to validate the `is_accessible_by` predicate against real serialized
    /// blocks without a live database. `outer @> inner` is true when every part of
    /// `inner` is present in `outer`: objects match by key/value subset, arrays
    /// match when each element of `inner` is contained in *some* element of
    /// `outer`, and scalars match by equality.
    fn jsonb_contains(outer: &Value, inner: &Value) -> bool {
        match (outer, inner) {
            (Value::Object(o), Value::Object(i)) => i
                .iter()
                .all(|(k, iv)| o.get(k).is_some_and(|ov| jsonb_contains(ov, iv))),
            (Value::Array(o), Value::Array(i)) => {
                i.iter().all(|iv| o.iter().any(|ov| jsonb_contains(ov, iv)))
            }
            _ => outer == inner,
        }
    }

    fn blocks_json(blocks: &[Block]) -> Value {
        serde_json::to_value(blocks).expect("serialize blocks")
    }

    #[test]
    fn predicate_matches_file_block_referencing_blob() {
        let id = BlobId::new();
        let blocks = vec![
            Block::text("see attachment"),
            Block::File {
                blob_id: id,
                kind: FileKind::Image,
                name: "p.png".into(),
                size: 12,
            },
        ];
        // Postgres would evaluate `blocks @> blob_ref_predicate(id)`.
        assert!(jsonb_contains(
            &blocks_json(&blocks),
            &blob_ref_predicate(id)
        ));
    }

    #[test]
    fn predicate_matches_voice_block_referencing_blob() {
        let id = BlobId::new();
        let blocks = vec![Block::Voice {
            blob_id: id,
            duration_ms: 800,
            transcript: None,
        }];
        assert!(jsonb_contains(
            &blocks_json(&blocks),
            &blob_ref_predicate(id)
        ));
    }

    #[test]
    fn predicate_rejects_blocks_referencing_a_different_blob() {
        let wanted = BlobId::new();
        let other = BlobId::new();
        let blocks = vec![
            Block::text("unrelated"),
            Block::File {
                blob_id: other,
                kind: FileKind::Document,
                name: "x".into(),
                size: 1,
            },
        ];
        // A blob id that appears in no block must not be considered referenced.
        assert!(!jsonb_contains(
            &blocks_json(&blocks),
            &blob_ref_predicate(wanted)
        ));
    }

    #[test]
    fn predicate_rejects_blocks_with_no_attachment() {
        let id = BlobId::new();
        let blocks = vec![Block::text("just text"), Block::text("more text")];
        assert!(!jsonb_contains(
            &blocks_json(&blocks),
            &blob_ref_predicate(id)
        ));
    }
}

#[cfg(test)]
mod scope_db_tests {
    use super::*;
    use aero_common::{Block, MessageId, RoomId, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("blob-scope-{id}"))
            .execute(pool)
            .await
            .expect("insert participant");
        id
    }

    async fn workspace_room(
        pool: &PgPool,
        actor: ParticipantId,
        label: &str,
    ) -> (WorkspaceId, RoomId) {
        let workspace = WorkspaceId::new();
        let mut tx = pool.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("Blob scope {label}"))
        .bind(format!("blob-scope-{label}-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert blob-scope workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .bind(WorkspaceRole::Owner.as_str())
        .execute(&mut *tx)
        .await
        .expect("insert blob-scope workspace member");
        tx.commit().await.expect("commit workspace fixture");

        let room = crate::RoomRepo::new(pool.clone())
            .create_in_workspace(
                workspace,
                aero_common::RoomKind::Channel,
                Some(format!("Blob scope room {label}")),
                actor,
            )
            .await
            .expect("insert blob-scope room")
            .id;
        (workspace, room)
    }

    async fn finalized_blob(
        repo: &BlobRepo,
        owner: ParticipantId,
        workspace: Option<WorkspaceId>,
        label: &str,
    ) -> Blob {
        let reserved = repo
            .reserve_in_scope(
                NewBlob {
                    owner_id: owner,
                    kind: FileKind::Document,
                    name: format!("{label}.txt"),
                    mime: "text/plain".into(),
                    size: 5,
                    sha256: Some(format!("{label}-{owner}")),
                    storage_key: "pending:test".into(),
                },
                workspace,
                Some("default"),
            )
            .await
            .expect("reserve test blob");
        repo.finalize(reserved.id, &format!("test:{label}"))
            .await
            .expect("finalize test blob")
            .expect("reservation exists")
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn placement_is_immutable_and_dedup_is_scope_bound() {
        let pool = pool();
        let owner = participant(&pool).await;
        let workspace = WorkspaceId::new();
        let other_workspace = WorkspaceId::new();
        let repo = BlobRepo::new(pool.clone());
        let digest = format!("scope-{owner}");
        let reserved = repo
            .reserve_in_scope(
                NewBlob {
                    owner_id: owner,
                    kind: FileKind::Document,
                    name: "scope.txt".into(),
                    mime: "text/plain".into(),
                    size: 5,
                    sha256: Some(digest.clone()),
                    storage_key: "pending:test".into(),
                },
                Some(workspace),
                Some("eu-west-1"),
            )
            .await
            .expect("reserve scoped blob");
        repo.finalize(reserved.id, "regional:key")
            .await
            .expect("finalize")
            .expect("reservation exists");

        assert_eq!(
            repo.storage_scope(reserved.id).await.expect("scope"),
            Some(BlobStorageScope {
                workspace_id: Some(workspace),
                storage_region: Some("eu-west-1".into()),
            })
        );
        assert!(repo
            .find_by_owner_sha256_in_scope(owner, &digest, Some(workspace), "eu-west-1")
            .await
            .expect("same scope lookup")
            .is_some());
        assert!(repo
            .find_by_owner_sha256_in_scope(owner, &digest, Some(workspace), "default")
            .await
            .expect("different region lookup")
            .is_none());
        assert!(repo
            .find_by_owner_sha256_in_scope(owner, &digest, Some(other_workspace), "eu-west-1",)
            .await
            .expect("different workspace lookup")
            .is_none());

        let mutation = sqlx::query("UPDATE blobs SET storage_region = 'default' WHERE id = $1")
            .bind(reserved.id.to_uuid())
            .execute(&pool)
            .await;
        assert!(mutation.is_err(), "placement trigger must reject mutation");

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn workspace_scope_blocks_cross_tenant_reuse_and_revoked_member_download() {
        let pool = pool();
        let owner = participant(&pool).await;
        let member = participant(&pool).await;
        let (workspace_a, room_a) = workspace_room(&pool, owner, "a").await;
        let (_workspace_b, room_b) = workspace_room(&pool, owner, "b").await;
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(workspace_a.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .expect("enroll scoped blob member");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room_a.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .expect("enroll scoped blob room member");
        let repo = BlobRepo::new(pool.clone());
        let scoped = finalized_blob(&repo, member, Some(workspace_a), "scoped").await;
        let legacy = finalized_blob(&repo, member, None, "legacy").await;
        let scoped_blocks = vec![Block::File {
            blob_id: scoped.id,
            kind: FileKind::Document,
            name: scoped.name.clone(),
            size: scoped.size,
        }];
        let legacy_blocks = vec![Block::File {
            blob_id: legacy.id,
            kind: FileKind::Document,
            name: legacy.name.clone(),
            size: legacy.size,
        }];

        assert!(
            repo.is_accessible_by(scoped.id, member).await.unwrap(),
            "active member may download a scoped upload"
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(
            BlobRepo::lock_message_attachments_in_tx(&mut tx, &scoped_blocks, member, room_a,)
                .await
                .unwrap(),
            "same-workspace attachment is valid"
        );
        assert!(
            !BlobRepo::lock_message_attachments_in_tx(&mut tx, &scoped_blocks, member, room_b,)
                .await
                .unwrap(),
            "membership in one tenant cannot move a scoped blob into another tenant"
        );
        assert!(
            !BlobRepo::lock_message_attachments_in_tx(&mut tx, &legacy_blocks, member, room_a,)
                .await
                .unwrap(),
            "legacy unscoped blobs cannot enter new messages in the member's tenant"
        );
        assert!(
            !BlobRepo::lock_message_attachments_in_tx(&mut tx, &legacy_blocks, member, room_b,)
                .await
                .unwrap(),
            "legacy unscoped blobs cannot bypass another tenant's residency boundary"
        );
        tx.rollback().await.unwrap();
        assert!(
            repo.is_accessible_by(legacy.id, member).await.unwrap(),
            "legacy unscoped member downloads remain read-only compatible"
        );

        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $3)",
        )
        .bind(workspace_a.to_uuid())
        .bind(member.to_uuid())
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .expect("deactivate scoped member");

        assert!(
            !repo.is_accessible_by(scoped.id, member).await.unwrap(),
            "deactivation revokes member download of workspace data"
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(
            !BlobRepo::lock_message_attachments_in_tx(&mut tx, &scoped_blocks, member, room_a,)
                .await
                .unwrap(),
            "deactivation also revokes attachment reuse"
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn database_fence_rejects_legacy_cross_scope_writes_and_pollution_downloads() {
        let pool = pool();
        let owner = participant(&pool).await;
        let viewer = participant(&pool).await;
        let (workspace_a, room_a) = workspace_room(&pool, owner, "db-fence-a").await;
        let (workspace_b, room_b) = workspace_room(&pool, owner, "db-fence-b").await;
        for statement in [
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        ] {
            let scope = if statement.contains("workspace_members") {
                workspace_b.to_uuid()
            } else {
                room_b.to_uuid()
            };
            sqlx::query(statement)
                .bind(scope)
                .bind(viewer.to_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }

        let repo = BlobRepo::new(pool.clone());
        let scoped = finalized_blob(&repo, owner, Some(workspace_a), "db-fence-scoped").await;
        let legacy = finalized_blob(&repo, owner, None, "db-fence-legacy").await;
        let scoped_json = serde_json::to_value([Block::File {
            blob_id: scoped.id,
            kind: FileKind::Document,
            name: scoped.name.clone(),
            size: scoped.size,
        }])
        .unwrap();
        let legacy_json = serde_json::to_value([Block::File {
            blob_id: legacy.id,
            kind: FileKind::Document,
            name: legacy.name.clone(),
            size: legacy.size,
        }])
        .unwrap();

        let same_workspace = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(same_workspace.to_uuid())
        .bind(room_a.to_uuid())
        .bind(owner.to_uuid())
        .bind(&scoped_json)
        .execute(&pool)
        .await
        .expect("the database fence accepts an exact workspace match");

        for (room, blocks, label) in [
            (room_b, &scoped_json, "cross-workspace scoped blob"),
            (room_a, &legacy_json, "unscoped legacy blob"),
        ] {
            let error = sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, blocks)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(MessageId::new().to_uuid())
            .bind(room.to_uuid())
            .bind(owner.to_uuid())
            .bind(blocks)
            .execute(&pool)
            .await
            .expect_err(label);
            let sqlstate = error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code);
            assert_eq!(
                sqlstate.as_deref(),
                Some("23514"),
                "{label} is rejected as a check violation"
            );
        }

        // Seed the shape that could have been written before the trigger existed.
        // DDL is transactional and the ACCESS EXCLUSIVE lock prevents another
        // test/session from observing the trigger-disabled interval.
        let polluted = MessageId::new();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE messages IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "ALTER TABLE messages
             DISABLE TRIGGER messages_enforce_blob_workspace_scope",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(polluted.to_uuid())
        .bind(room_b.to_uuid())
        .bind(owner.to_uuid())
        .bind(&scoped_json)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "ALTER TABLE messages
             ENABLE TRIGGER messages_enforce_blob_workspace_scope",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert!(
            !repo.is_accessible_by(scoped.id, viewer).await.unwrap(),
            "a historical polluted reference cannot grant a tenant-B member blob-A bytes"
        );
    }
}
