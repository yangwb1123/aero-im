// Pure reducers for the per-message "seen by" and interactive-action feed.

export function mergeSeenReader(readers, participantId) {
  const next = Array.isArray(readers) ? readers.slice() : [];
  if (participantId && !next.includes(participantId)) next.push(participantId);
  return next;
}

export function mergeInteraction(interactions, event) {
  const next = Array.isArray(interactions) ? interactions.slice() : [];
  if (!event?.participant || !event?.action_id) return next;
  const key = `${event.participant}\u0000${event.action_id}`;
  if (!next.some((item) => item.key === key)) {
    next.push({ key, participant: event.participant, actionId: event.action_id });
  }
  return next;
}

export function participantLabel(participants, participantId) {
  const name = participants?.get?.(participantId)?.display_name;
  return name || (participantId ? `${String(participantId).slice(0, 6)}…` : '未知成员');
}
