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
