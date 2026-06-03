//! Stream VOD / recording repository (save a finished live stream for replay).
//!
//! Backs `migrations/0025_vod.sql`. When a streamer flags a stream for recording
//! and it later ends, the HLS playlist already written by the ingest pipeline
//! (under the configured `hls_dir`, pointed at by `streams.hls_path`) is retained
//! as a durable [`Vod`] that room members can list and play back later.
//!
//! Purely additive: a NEW [`VodRepo`]; no existing repo is touched. The per-stream
//! `recording` flag lives on the `streams` table (added by 0025); we read/write it
//! here via a raw `UPDATE`/`SELECT` rather than reaching into [`StreamRepo`], so
//! that module stays untouched. The `id` is a ULID stored as UUID (time-sortable),
//! like the audit trail's.
//!
//! NOTE on scope: this repository owns the recording *lifecycle* + VOD metadata.
//! Real media/segment capture rides the EXISTING HLS writer (the documented seam,
//! already byte-tested for WHIP) and is **not** performed here — a VOD references
//! the playlist the live pipeline produced.

use aero_common::{ParticipantId, RoomId, Vod, VodId};
use sqlx::PgPool;

/// Default page size for a VOD listing when the caller does not specify one.
const DEFAULT_LIMIT: i64 = 100;
/// Hard ceiling on a VOD listing page, keeping the scan bounded.
const MAX_LIMIT: i64 = 200;

/// Clamp a requested page size into `1..=MAX_LIMIT` (defaulting `None`/non-positive
/// to [`DEFAULT_LIMIT`]). Pure so it unit-tests without a DB.
fn clamp_limit(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

/// Render an absolute playback URL for a VOD's HLS playlist.
///
/// Joins `public_base` (e.g. `https://aero.example`) with the playlist path,
/// which is served by the static `/hls/...` mount. `hls_path` may already be a
/// `/hls/...`-rooted path (as the live pipeline writes), an `hls/...` path, or a
/// bare per-stream path — all normalize to a single `…/hls/<rest>` URL with no
/// doubled slashes. Pure, so it is unit-tested offline.
#[must_use]
pub fn playback_url(public_base: &str, hls_path: &str) -> String {
    let base = public_base.trim_end_matches('/');
    // Normalize the stored path to the segment that follows `/hls/`.
    let rest = hls_path
        .trim_start_matches('/')
        .strip_prefix("hls/")
        .unwrap_or_else(|| hls_path.trim_start_matches('/'));
    format!("{base}/hls/{rest}")
}

#[derive(Clone)]
pub struct VodRepo {
    pool: PgPool,
}

impl VodRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a finalized recording, returning its generated id. The caller has
    /// already resolved the source stream (and its `hls_path`) and checked owner.
    pub async fn create(
        &self,
        stream_id: ulid::Ulid,
        owner: ParticipantId,
        room: Option<RoomId>,
        title: &str,
        hls_path: &str,
        duration_secs: Option<u32>,
    ) -> Result<VodId, sqlx::Error> {
        let id = VodId::new();
        let created_at = time::OffsetDateTime::now_utc();
        // `INTEGER` column: cap at i32::MAX rather than truncate/over/underflow.
        let dur: Option<i32> = duration_secs.map(|d| i32::try_from(d).unwrap_or(i32::MAX));
        sqlx::query(
            r"INSERT INTO stream_recordings
                  (id, stream_id, owner_id, room_id, title, hls_path, duration_secs, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.to_uuid())
        .bind(uuid::Uuid::from_u128(stream_id.0))
        .bind(owner.to_uuid())
        .bind(room.map(|r| r.to_uuid()))
        .bind(title)
        .bind(hls_path)
        .bind(dur)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// One VOD by id, if it exists.
    pub async fn get(&self, id: VodId) -> Result<Option<Vod>, sqlx::Error> {
        let row = sqlx::query_as::<_, VodRow>(
            r"SELECT id, stream_id, owner_id, room_id, title, hls_path, duration_secs, created_at
               FROM stream_recordings WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Vod::from))
    }

    /// A room's recordings, newest first. `limit` is clamped into `[1, MAX_LIMIT]`.
    pub async fn list_for_room(
        &self,
        room: RoomId,
        limit: Option<i64>,
    ) -> Result<Vec<Vod>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, VodRow>(
            r"SELECT id, stream_id, owner_id, room_id, title, hls_path, duration_secs, created_at
               FROM stream_recordings
               WHERE room_id = $1
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(room.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Vod::from).collect())
    }

    /// An owner's recordings, newest first. `limit` is clamped into `[1, MAX_LIMIT]`.
    pub async fn list_for_owner(
        &self,
        owner: ParticipantId,
        limit: Option<i64>,
    ) -> Result<Vec<Vod>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, VodRow>(
            r"SELECT id, stream_id, owner_id, room_id, title, hls_path, duration_secs, created_at
               FROM stream_recordings
               WHERE owner_id = $1
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(owner.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Vod::from).collect())
    }

    /// Delete a VOD, scoped to its owner (a non-owner's delete affects 0 rows).
    /// Returns `true` if a row was removed.
    pub async fn delete(&self, id: VodId, owner: ParticipantId) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query(r"DELETE FROM stream_recordings WHERE id = $1 AND owner_id = $2")
                .bind(id.to_uuid())
                .bind(owner.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Set (or clear) a stream's `recording` flag. Raw `UPDATE` on `streams` so
    /// [`StreamRepo`](crate::StreamRepo) stays untouched. Returns `true` if the
    /// stream existed (a row was updated).
    pub async fn set_recording(
        &self,
        stream_id: ulid::Ulid,
        on: bool,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"UPDATE streams SET recording = $2 WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream_id.0))
            .bind(on)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Read a stream's `recording` flag. `None` when the stream does not exist.
    pub async fn is_recording(&self, stream_id: ulid::Ulid) -> Result<Option<bool>, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(r"SELECT recording FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream_id.0))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }
}

#[derive(sqlx::FromRow)]
struct VodRow {
    id: uuid::Uuid,
    stream_id: uuid::Uuid,
    owner_id: uuid::Uuid,
    room_id: Option<uuid::Uuid>,
    title: String,
    hls_path: String,
    duration_secs: Option<i32>,
    created_at: time::OffsetDateTime,
}

impl From<VodRow> for Vod {
    fn from(r: VodRow) -> Self {
        Self {
            id: VodId::from_uuid(r.id),
            stream_id: ulid::Ulid(r.stream_id.as_u128()),
            owner_id: ParticipantId::from_uuid(r.owner_id),
            room_id: r.room_id.map(RoomId::from_uuid),
            title: r.title,
            hls_path: r.hls_path,
            // A negative stored value (shouldn't happen) maps to None rather than
            // panicking; non-negative i32 always fits u32.
            duration_secs: r.duration_secs.and_then(|d| u32::try_from(d).ok()),
            created_at: r.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(-5)), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn playback_url_joins_base_and_hls_rooted_path() {
        // The live pipeline writes `/hls/<stream>/index.m3u8`.
        assert_eq!(
            playback_url("https://aero.example", "/hls/abc/index.m3u8"),
            "https://aero.example/hls/abc/index.m3u8"
        );
    }

    #[test]
    fn playback_url_trims_trailing_base_slash_and_avoids_doubling() {
        assert_eq!(
            playback_url("https://aero.example/", "/hls/abc/index.m3u8"),
            "https://aero.example/hls/abc/index.m3u8"
        );
    }

    #[test]
    fn playback_url_handles_paths_without_hls_prefix() {
        // A bare per-stream path (no `hls/` prefix) is mounted under `/hls/`.
        assert_eq!(
            playback_url("https://aero.example", "abc/index.m3u8"),
            "https://aero.example/hls/abc/index.m3u8"
        );
        assert_eq!(
            playback_url("https://aero.example", "/abc/index.m3u8"),
            "https://aero.example/hls/abc/index.m3u8"
        );
    }

    #[test]
    fn playback_url_handles_hls_prefix_without_leading_slash() {
        assert_eq!(
            playback_url("http://localhost:8080", "hls/abc/index.m3u8"),
            "http://localhost:8080/hls/abc/index.m3u8"
        );
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored vod_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// A self-contained (owner, room, stream) triple so the VOD tests stand alone.
    /// Returns the owner, the room, and the source stream's id.
    async fn fixture(p: &PgPool) -> (ParticipantId, RoomId, ulid::Ulid) {
        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(owner.to_uuid())
            .bind(format!("vod-owner-{owner}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("vod-room")
        .bind(owner.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let stream_id = ulid::Ulid::new();
        sqlx::query(
            "INSERT INTO streams (id, owner_id, room_id, title, stream_key, status, protocol, created_at)
             VALUES ($1,$2,$3,$4,$5,'ended','whip', now())",
        )
        .bind(uuid::Uuid::from_u128(stream_id.0))
        .bind(owner.to_uuid())
        .bind(room.to_uuid())
        .bind("vod-stream")
        .bind(format!("key-{stream_id}"))
        .execute(p)
        .await
        .expect("insert stream");
        (owner, room, stream_id)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn vod_create_list_get_delete_roundtrip() {
        let p = pool();
        let repo = VodRepo::new(p.clone());
        let (owner, room, stream_id) = fixture(&p).await;

        let hls = format!("/hls/{stream_id}/index.m3u8");
        let id = repo
            .create(stream_id, owner, Some(room), "My VOD", &hls, Some(3600))
            .await
            .unwrap();

        // list_for_room returns it.
        let by_room = repo.list_for_room(room, Some(10)).await.unwrap();
        assert_eq!(by_room.len(), 1, "one VOD in the room");
        assert_eq!(by_room[0].id, id);
        assert_eq!(by_room[0].title, "My VOD");
        assert_eq!(by_room[0].hls_path, hls);
        assert_eq!(by_room[0].duration_secs, Some(3600));

        // list_for_owner returns it too.
        let by_owner = repo.list_for_owner(owner, None).await.unwrap();
        assert!(by_owner.iter().any(|v| v.id == id), "owner sees their VOD");

        // get returns the same record.
        let got = repo.get(id).await.unwrap().expect("VOD exists");
        assert_eq!(got.id, id);
        assert_eq!(got.stream_id, stream_id);
        assert_eq!(got.owner_id, owner);

        // A non-owner delete is a no-op; the owner's delete removes it.
        let stranger = ParticipantId::new();
        assert!(!repo.delete(id, stranger).await.unwrap(), "non-owner cannot delete");
        assert!(repo.get(id).await.unwrap().is_some(), "still present");
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(repo.get(id).await.unwrap().is_none(), "gone after delete");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn vod_recording_flag_roundtrip() {
        let p = pool();
        let repo = VodRepo::new(p.clone());
        let (_owner, _room, stream_id) = fixture(&p).await;

        // Default is false (column default).
        assert_eq!(repo.is_recording(stream_id).await.unwrap(), Some(false));
        assert!(repo.set_recording(stream_id, true).await.unwrap(), "stream existed");
        assert_eq!(repo.is_recording(stream_id).await.unwrap(), Some(true));
        assert!(repo.set_recording(stream_id, false).await.unwrap());
        assert_eq!(repo.is_recording(stream_id).await.unwrap(), Some(false));

        // An unknown stream reports None / "no row updated".
        let unknown = ulid::Ulid::new();
        assert_eq!(repo.is_recording(unknown).await.unwrap(), None);
        assert!(!repo.set_recording(unknown, true).await.unwrap());
    }
}
