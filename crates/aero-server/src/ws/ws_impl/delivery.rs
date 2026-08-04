//! Durable delivery-cursor v2 frame handling.

use crate::state::AppState;
use aero_common::{MessageId, ParticipantId, RoomId};
use axum::extract::ws::Message;
use tokio::sync::mpsc;

use super::ServerFrame;

pub(super) async fn handle_ack(
    state: &AppState,
    participant: ParticipantId,
    tx: &mpsc::Sender<Message>,
    room: RoomId,
    message: MessageId,
    delivery_ordinal: Option<i64>,
    seq: i64,
) -> anyhow::Result<()> {
    // Delivery state is room data, so use the same effective-access boundary as
    // history reads. The repository additionally proves that the acknowledged
    // message and ordinal belong to this exact room.
    state.im.assert_room_access(participant, room).await?;
    let Some(delivery_ordinal) = delivery_ordinal else {
        let _ = tx.try_send(Message::Text(
            serde_json::to_string(&ServerFrame::Error {
                code: "delivery_cursor_version",
                msg: "delivery_ack requires delivery_ordinal".into(),
            })
            .unwrap_or_default(),
        ));
        return Ok(());
    };
    if let Err(error) = state
        .delivery_cursors
        .advance(participant, room, message, delivery_ordinal, seq)
        .await
    {
        // Best-effort persistence: a transient failure only causes extra replay
        // next reconnect and must not tear down the socket.
        tracing::warn!(
            ?error,
            %participant,
            %room,
            delivery_ordinal,
            "delivery_ack persist failed"
        );
    }
    Ok(())
}
