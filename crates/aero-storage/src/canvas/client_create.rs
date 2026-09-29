//! Retry-safe channel-canvas creation keyed by the caller's stable operation id.

use aero_common::{CanvasId, Error, ParticipantId, RoomId};

use super::{
    lock_live_channel_access, row_to_model, validate_blocks, validate_title, Canvas, CanvasRepo,
    Row, COLUMNS,
};

fn validate_client_create_id(client_create_id: uuid::Uuid) -> Result<CanvasId, Error> {
    if client_create_id.get_version_num() != 7 {
        return Err(Error::Invalid("client_create_id must be a UUIDv7".into()));
    }
    Ok(CanvasId::from_uuid(client_create_id))
}

pub(super) async fn create(
    repo: &CanvasRepo,
    room: RoomId,
    author: ParticipantId,
    client_create_id: uuid::Uuid,
    title: &str,
    blocks: &serde_json::Value,
) -> Result<Canvas, Error> {
    let id = validate_client_create_id(client_create_id)?;
    let title = validate_title(title)?;
    validate_blocks(blocks)?;
    let mut tx = repo.pool.begin().await?;
    lock_live_channel_access(&mut tx, room, author).await?;
    // UUIDv7's timestamp occupies the high 48 bits, matching the ULID ordering
    // used by CanvasId; the remaining random bits retain retry uniqueness.
    let sql = format!(
        "INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO NOTHING
         RETURNING {COLUMNS}"
    );
    let inserted = sqlx::query_as::<_, Row>(&sql)
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(author.to_uuid())
        .bind(title)
        .bind(blocks)
        .fetch_optional(&mut *tx)
        .await?;
    let row = if let Some(row) = inserted {
        row
    } else {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM channel_canvases
              WHERE id = $1 AND room_id = $2 AND author_id = $3"
        );
        sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(author.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::Conflict("canvas create identity already in use".into()))?
    };
    tx.commit().await?;
    Ok(row_to_model(row))
}

#[cfg(test)]
mod tests {
    use super::validate_client_create_id;

    #[test]
    fn create_identity_must_be_uuid_v7_to_preserve_ulid_ordering() {
        let client_create_id = uuid::Uuid::now_v7();
        let canvas_id = validate_client_create_id(client_create_id).unwrap();
        assert_eq!(canvas_id.to_uuid(), client_create_id);
        assert!(validate_client_create_id(uuid::Uuid::new_v4()).is_err());
    }
}
