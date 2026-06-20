
use std::{collections::HashMap, str::FromStr};

use aero_common::{
    Error as AeroError, MessageId, Result as AeroResult, RoomId, RoomKind,
};
use aero_storage::SearchHit;

// ----- helpers -----

pub(crate) fn parse_room_kind(s: &str) -> AeroResult<RoomKind> {
    match s {
        "direct" => Ok(RoomKind::Direct),
        "group" => Ok(RoomKind::Group),
        "channel" => Ok(RoomKind::Channel),
        _ => Err(AeroError::Invalid(format!("unknown room kind: {s}"))),
    }
}

/// Merge two SearchHit lists (FTS + vector). Dedupes by message id, takes the
/// max score per id, returns top `limit` ordered by score desc.
pub(crate) fn merge_hits(
    a: Vec<aero_storage::SearchHit>,
    b: Vec<aero_storage::SearchHit>,
    limit: i64,
) -> Vec<aero_storage::SearchHit> {
    use std::collections::HashMap;
    let mut best: HashMap<MessageId, aero_storage::SearchHit> = HashMap::new();
    for h in a.into_iter().chain(b.into_iter()) {
        let id = h.message.id;
        match best.get(&id) {
            Some(existing) if existing.score >= h.score => {}
            _ => {
                best.insert(id, h);
            }
        }
    }
    let mut out: Vec<_> = best.into_values().collect();
    out.sort_by(|x, y| y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal));
    let limit = limit.clamp(1, 100) as usize;
    out.truncate(limit);
    out
}

pub(crate) fn parse_room_id(s: &str) -> AeroResult<RoomId> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

