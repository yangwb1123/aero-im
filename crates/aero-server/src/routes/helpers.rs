use std::str::FromStr;

use aero_common::{
    Error as AeroError, MessageId, ParticipantId, Result as AeroResult, RoomId, RoomKind,
    WorkspaceId,
};

use crate::state::AppState;

// ----- helpers -----

pub(crate) fn parse_room_kind(s: &str) -> AeroResult<RoomKind> {
    match s {
        "direct" => Ok(RoomKind::Direct),
        "group" => Ok(RoomKind::Group),
        "channel" => Ok(RoomKind::Channel),
        _ => Err(AeroError::Invalid(format!("unknown room kind: {s}"))),
    }
}

/// Merge two `SearchHit` lists (FTS + vector). Dedupes by message id, takes the
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
    out.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let limit = usize::try_from(limit.clamp(1, 100)).expect("clamped to 1..=100");
    out.truncate(limit);
    out
}

pub(crate) fn parse_room_id(s: &str) -> AeroResult<RoomId> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Assert that `participant` may currently enter a workspace-scoped request.
///
/// A retained `workspace_members` row is not sufficient authorization: deleted
/// accounts, workspace deactivations, and members who have not satisfied a
/// mandatory-2FA policy are all rejected by `effective_member_role`.
pub(crate) async fn assert_effective_workspace_member(
    state: &AppState,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> AeroResult<()> {
    state
        .workspaces
        .effective_member_role(workspace, participant)
        .await
        .map_err(AeroError::from)?
        .map(|_| ())
        .ok_or_else(|| AeroError::Forbidden("not an active workspace member".into()))
}

/// Batch form of [`assert_effective_workspace_member`], used when a client-
/// supplied operation would expose a new conversation to several participants.
/// The repository evaluates the entire set with one effective-access query.
pub(crate) async fn assert_effective_workspace_members(
    state: &AppState,
    workspace: WorkspaceId,
    participants: &[ParticipantId],
) -> AeroResult<()> {
    if state
        .workspaces
        .all_effective_members(workspace, participants)
        .await
        .map_err(AeroError::from)?
    {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "every conversation member must be an active workspace member".into(),
        ))
    }
}
