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
