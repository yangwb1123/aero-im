//! Blob repository — metadata for attachments stored in S3/MinIO.
//!
//! The bytes themselves live in object storage; this table tracks the metadata
//! and gates access (only owner + room members can fetch the binary payload).

use aero_common::{Blob, BlobId, FileKind, ParticipantId};
use sqlx::PgPool;

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

impl BlobRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, new: NewBlob) -> Result<Blob, sqlx::Error> {
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
                 (id, owner_id, kind, name, mime, size, sha256, storage_key, created_at, finalized_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        )
        .bind(id.to_uuid())
        .bind(new.owner_id.to_uuid())
        .bind(kind_s)
        .bind(&new.name)
        .bind(&new.mime)
        .bind(size_i)
        .bind(new.sha256.as_deref())
        .bind(&new.storage_key)
        .bind(created_at)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(Blob {
            id,
            owner_id: new.owner_id,
            kind: new.kind,
            name: new.name,
            mime: new.mime,
            size: new.size,
            sha256: new.sha256,
            storage_key: new.storage_key,
            created_at,
            finalized_at: Some(created_at),
        })
    }

    pub async fn get(&self, id: BlobId) -> Result<Option<Blob>, sqlx::Error> {
        let row = sqlx::query_as::<_, BlobRow>(
            r#"SELECT id, owner_id, kind, name, mime, size, sha256, storage_key, created_at, finalized_at
               FROM blobs WHERE id = $1"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Blob::from))
    }

    /// Authorization check for blob download (IDOR guard).
    ///
    /// A blob has no direct room column; it is linked to rooms only by being
    /// referenced from a `File`/`Voice` block (`{"blob_id": "<ulid>"}`) inside a
    /// message's `blocks` JSONB. A participant may access a blob when either:
    ///
    /// * they uploaded it (`blobs.owner_id`), or
    /// * it is referenced by a message in a room they belong to.
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
                   -- uploader always has access
                   SELECT 1 FROM blobs b
                   WHERE b.id = $1 AND b.owner_id = $2
                   UNION ALL
                   -- or the blob is referenced by a message in a room the viewer is in
                   SELECT 1
                   FROM messages m
                   JOIN room_members rm ON rm.room_id = m.room_id
                   WHERE rm.participant_id = $2
                     AND m.blocks @> $3
               ) AS ok",
        )
        .bind(id.to_uuid())
        .bind(viewer.to_uuid())
        .bind(blob_ref)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }
}

#[derive(sqlx::FromRow)]
struct BlobRow {
    id: uuid::Uuid,
    owner_id: uuid::Uuid,
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
            (Value::Array(o), Value::Array(i)) => i
                .iter()
                .all(|iv| o.iter().any(|ov| jsonb_contains(ov, iv))),
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
            Block::File { blob_id: id, kind: FileKind::Image, name: "p.png".into(), size: 12 },
        ];
        // Postgres would evaluate `blocks @> blob_ref_predicate(id)`.
        assert!(jsonb_contains(&blocks_json(&blocks), &blob_ref_predicate(id)));
    }

    #[test]
    fn predicate_matches_voice_block_referencing_blob() {
        let id = BlobId::new();
        let blocks = vec![Block::Voice { blob_id: id, duration_ms: 800, transcript: None }];
        assert!(jsonb_contains(&blocks_json(&blocks), &blob_ref_predicate(id)));
    }

    #[test]
    fn predicate_rejects_blocks_referencing_a_different_blob() {
        let wanted = BlobId::new();
        let other = BlobId::new();
        let blocks = vec![
            Block::text("unrelated"),
            Block::File { blob_id: other, kind: FileKind::Document, name: "x".into(), size: 1 },
        ];
        // A blob id that appears in no block must not be considered referenced.
        assert!(!jsonb_contains(&blocks_json(&blocks), &blob_ref_predicate(wanted)));
    }

    #[test]
    fn predicate_rejects_blocks_with_no_attachment() {
        let id = BlobId::new();
        let blocks = vec![Block::text("just text"), Block::text("more text")];
        assert!(!jsonb_contains(&blocks_json(&blocks), &blob_ref_predicate(id)));
    }
}
