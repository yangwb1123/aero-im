use aero_common::{MessageId, ParticipantId, SavedSearchId};

const DELIVERY_NAMESPACE: uuid::Uuid = uuid::uuid!("50d1ee4d-95f8-4ba4-a6b8-c889aa7aaf32");
const LOOKBACK: time::Duration = time::Duration::minutes(15);

pub(super) fn delivery_id(
    search: SavedSearchId,
    message: MessageId,
    owner: ParticipantId,
) -> uuid::Uuid {
    let mut name = [0_u8; 48];
    name[..16].copy_from_slice(search.to_uuid().as_bytes());
    name[16..32].copy_from_slice(message.to_uuid().as_bytes());
    name[32..].copy_from_slice(owner.to_uuid().as_bytes());
    uuid::Uuid::new_v5(&DELIVERY_NAMESPACE, &name)
}

pub(super) fn lower_bound(
    cursor_at: time::OffsetDateTime,
    floor_at: time::OffsetDateTime,
    floor_message_id: Option<uuid::Uuid>,
) -> (time::OffsetDateTime, Option<uuid::Uuid>) {
    let overlap_at = cursor_at - LOOKBACK;
    if overlap_at <= floor_at {
        (floor_at, floor_message_id)
    } else {
        (overlap_at, Some(uuid::Uuid::nil()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_key_is_stable_and_binds_every_identity() {
        let search = SavedSearchId::new();
        let message = MessageId::new();
        let owner = ParticipantId::new();
        let stable = delivery_id(search, message, owner);
        assert_eq!(stable, delivery_id(search, message, owner));
        assert_ne!(stable, delivery_id(search, MessageId::new(), owner));
        assert_ne!(stable, delivery_id(search, message, ParticipantId::new()));
        assert_ne!(stable, delivery_id(SavedSearchId::new(), message, owner));
    }

    #[test]
    fn lookback_never_crosses_enable_floor_and_uses_nil_tiebreaker() {
        let floor = time::OffsetDateTime::UNIX_EPOCH;
        let floor_id = uuid::Uuid::new_v4();
        assert_eq!(
            lower_bound(floor + time::Duration::minutes(5), floor, Some(floor_id)),
            (floor, Some(floor_id))
        );
        assert_eq!(
            lower_bound(floor + time::Duration::minutes(20), floor, Some(floor_id)),
            (floor + time::Duration::minutes(5), Some(uuid::Uuid::nil()))
        );
    }
}
