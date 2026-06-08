//! Live-stream repository (P4/P5).

use aero_common::{ParticipantId, RoomId, Stream, StreamProtocol, StreamStatus};
use sqlx::PgPool;
use ulid::Ulid;

#[derive(Clone)]
pub struct StreamRepo {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewStream {
    pub owner_id: ParticipantId,
    pub room_id: Option<RoomId>,
    pub title: String,
    pub protocol: StreamProtocol,
    /// Stream key the publisher uses (e.g. RTMP path component). Random if `None`.
    pub stream_key: Option<String>,
}

impl StreamRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, new: NewStream) -> Result<Stream, sqlx::Error> {
        let id = Ulid::new();
        let created_at = time::OffsetDateTime::now_utc();
        let proto = match new.protocol {
            StreamProtocol::Rtmp => "rtmp",
            StreamProtocol::Whip => "whip",
            StreamProtocol::Srt => "srt",
        };
        let key = new.stream_key.unwrap_or_else(random_key);
        sqlx::query(
            r#"INSERT INTO streams (id, owner_id, room_id, title, stream_key, status, protocol, created_at)
               VALUES ($1, $2, $3, $4, $5, 'idle', $6, $7)"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(new.owner_id.to_uuid())
        .bind(new.room_id.map(|r| r.to_uuid()))
        .bind(&new.title)
        .bind(&key)
        .bind(proto)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(Stream {
            id,
            owner_id: new.owner_id,
            room_id: new.room_id,
            title: new.title,
            stream_key: key,
            status: StreamStatus::Idle,
            hls_path: None,
            protocol: new.protocol,
            started_at: None,
            ended_at: None,
            created_at,
        })
    }

    /// Look up by stream_key — used by RTMP ingest at publish-time.
    pub async fn get_by_key(&self, key: &str) -> Result<Option<Stream>, sqlx::Error> {
        let row = sqlx::query_as::<_, StreamRow>(
            r#"SELECT id, owner_id, room_id, title, stream_key, status, hls_path, protocol,
                      started_at, ended_at, created_at
               FROM streams WHERE stream_key = $1"#,
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Stream::from))
    }

    pub async fn get(&self, id: Ulid) -> Result<Option<Stream>, sqlx::Error> {
        let row = sqlx::query_as::<_, StreamRow>(
            r#"SELECT id, owner_id, room_id, title, stream_key, status, hls_path, protocol,
                      started_at, ended_at, created_at
               FROM streams WHERE id = $1"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Stream::from))
    }

    /// Rotate (reset) a stream's secret `stream_key` to a freshly generated one,
    /// owner-scoped: the update only matches when `id` belongs to `owner`, so a
    /// non-owner (or a missing stream) leaves the row untouched and yields `None`.
    /// Returns the new key on success. A leaked key can thus be invalidated
    /// without recreating the stream (standard Twitch/YouTube control).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn rotate_key(
        &self,
        id: Ulid,
        owner: ParticipantId,
    ) -> Result<Option<String>, sqlx::Error> {
        let new_key = random_key();
        let row: Option<(String,)> = sqlx::query_as(
            r"UPDATE streams SET stream_key = $1
               WHERE id = $2 AND owner_id = $3
            RETURNING stream_key",
        )
        .bind(&new_key)
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(owner.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(k,)| k))
    }

    /// Change a stream's human-facing `title` (e.g. editing it while live —
    /// the standard Twitch/YouTube "edit stream info" control). The `streams`
    /// table carries no other free-text metadata column (description/category
    /// live in separate tables, see migrations 0050), so only `title` is touched.
    ///
    /// Returns `true` when a row matched (the stream exists), `false` otherwise.
    /// Ownership is enforced by the caller (see `crate::stream_meta`), mirroring
    /// the owner check pattern in the HTTP layer; this method is unscoped on
    /// purpose so the handler can return `404` vs `403` distinctly.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn update_title(&self, stream: Ulid, title: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(r"UPDATE streams SET title = $2 WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.0))
            .bind(title)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_live(&self, id: Ulid, hls_path: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE streams
                  SET status = 'live', started_at = NOW(), hls_path = $2
               WHERE id = $1"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(hls_path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_ended(&self, id: Ulid) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE streams SET status = 'ended', ended_at = NOW() WHERE id = $1"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_live(&self) -> Result<Vec<Stream>, sqlx::Error> {
        let rows = sqlx::query_as::<_, StreamRow>(
            r#"SELECT id, owner_id, room_id, title, stream_key, status, hls_path, protocol,
                      started_at, ended_at, created_at
               FROM streams WHERE status = 'live' ORDER BY started_at DESC NULLS LAST"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Stream::from).collect())
    }
}

fn random_key() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::thread_rng().gen();
    hex_encode(&bytes)
}

fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

#[derive(sqlx::FromRow)]
struct StreamRow {
    id: uuid::Uuid,
    owner_id: uuid::Uuid,
    room_id: Option<uuid::Uuid>,
    title: String,
    stream_key: String,
    status: String,
    hls_path: Option<String>,
    protocol: String,
    started_at: Option<time::OffsetDateTime>,
    ended_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored rotate_key
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway owner participant so the test is self-contained.
    async fn owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("stream-key-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn rotate_key_changes_key_and_is_owner_scoped() {
        let p = pool();
        let repo = StreamRepo::new(p.clone());
        let owner = owner(&p).await;
        let stranger = ParticipantId::new();

        let stream = repo
            .create(NewStream {
                owner_id: owner,
                room_id: None,
                title: "rotate-key test".into(),
                protocol: StreamProtocol::Rtmp,
                stream_key: None,
            })
            .await
            .expect("create stream");
        let original = stream.stream_key.clone();

        // A non-owner cannot rotate the key: no row matches ⇒ None, key unchanged.
        assert!(
            repo.rotate_key(stream.id, stranger)
                .await
                .expect("rotate (stranger)")
                .is_none(),
            "stranger cannot rotate another owner's stream key"
        );
        let after_stranger = repo.get(stream.id).await.expect("get").expect("present");
        assert_eq!(after_stranger.stream_key, original, "key untouched by stranger");

        // The owner rotates: a NEW key is returned and persisted.
        let rotated = repo
            .rotate_key(stream.id, owner)
            .await
            .expect("rotate (owner)")
            .expect("owner rotates");
        assert_ne!(rotated, original, "rotated key differs from the original");
        let reread = repo.get(stream.id).await.expect("get").expect("present");
        assert_eq!(reread.stream_key, rotated, "persisted key matches the returned one");

        // The old key no longer resolves; the new one does.
        assert!(
            repo.get_by_key(&original).await.expect("by old key").is_none(),
            "the leaked key is invalidated"
        );
        assert_eq!(
            repo.get_by_key(&rotated).await.expect("by new key").map(|s| s.id),
            Some(stream.id),
            "the new key resolves to the stream"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM streams WHERE owner_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn update_title_persists_and_reports_match() {
        let p = pool();
        let repo = StreamRepo::new(p.clone());
        let owner = owner(&p).await;

        let stream = repo
            .create(NewStream {
                owner_id: owner,
                room_id: None,
                title: "original title".into(),
                protocol: StreamProtocol::Rtmp,
                stream_key: None,
            })
            .await
            .expect("create stream");

        // Updating an existing stream reports a match and persists the new title.
        let matched = repo.update_title(stream.id, "renamed live").await.expect("update");
        assert!(matched, "an existing stream's title update matches a row");
        let reread = repo.get(stream.id).await.expect("get").expect("present");
        assert_eq!(reread.title, "renamed live", "the new title is persisted");

        // Updating a non-existent stream matches no row.
        let missing = repo
            .update_title(Ulid::new(), "ghost")
            .await
            .expect("update (missing)");
        assert!(!missing, "an unknown stream id matches no row");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM streams WHERE owner_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}

impl From<StreamRow> for Stream {
    fn from(r: StreamRow) -> Self {
        let status = match r.status.as_str() {
            "live" => StreamStatus::Live,
            "ended" => StreamStatus::Ended,
            _ => StreamStatus::Idle,
        };
        let protocol = match r.protocol.as_str() {
            "whip" => StreamProtocol::Whip,
            "srt" => StreamProtocol::Srt,
            _ => StreamProtocol::Rtmp,
        };
        Self {
            id: Ulid(r.id.as_u128()),
            owner_id: ParticipantId::from_uuid(r.owner_id),
            room_id: r.room_id.map(RoomId::from_uuid),
            title: r.title,
            stream_key: r.stream_key,
            status,
            hls_path: r.hls_path,
            protocol,
            started_at: r.started_at,
            ended_at: r.ended_at,
            created_at: r.created_at,
        }
    }
}
