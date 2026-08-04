// Pure StreamEvent dispatcher. Kept DOM-free so every backend event variant is
// covered by the node test gate instead of silently falling through in the SPA.

function invoke(controller, method, event) {
  if (typeof controller?.[method] !== 'function') return false;
  controller[method](event);
  return true;
}

export function dispatchStreamEvent(controller, event) {
  if (!controller || !event?.kind || !event.stream_id) return false;
  switch (event.kind) {
    case 'chat': return invoke(controller, 'addChat', event);
    case 'gift': return invoke(controller, 'addGift', event);
    case 'viewers':
      if (typeof controller.setViewers !== 'function') return false;
      controller.setViewers(event.count);
      return true;
    case 'status':
      if (typeof controller.setStatus !== 'function') return false;
      controller.setStatus(event.status);
      return true;
    case 'hype_train': return invoke(controller, 'setHypeTrain', event);
    case 'raid': return invoke(controller, 'showRaid', event);
    case 'points_redeemed': return invoke(controller, 'showPointsRedemption', event);
    case 'goal_progress': return invoke(controller, 'setGoalProgress', event);
    case 'goal_reached': return invoke(controller, 'showGoalReached', event);
    case 'prediction_opened':
    case 'prediction_locked':
    case 'prediction_resolved':
      return invoke(controller, 'updatePrediction', event);
    default: return false;
  }
}
