import test from 'node:test';
import assert from 'node:assert/strict';

import { dispatchStreamEvent } from './stream_events.js';
import { mergeInteraction, mergeSeenReader, participantLabel } from './message_activity_model.js';

test('every StreamEvent variant reaches its card controller', () => {
  const calls = [];
  const controller = {
    addChat: (event) => calls.push(['chat', event.body]),
    addGift: (event) => calls.push(['gift', event.gift_id]),
    setViewers: (count) => calls.push(['viewers', count]),
    setStatus: (status) => calls.push(['status', status]),
    setHypeTrain: (event) => calls.push(['hype', event.level]),
    showRaid: (event) => calls.push(['raid', event.target_stream_id]),
    showPointsRedemption: (event) => calls.push(['points', event.reward_id]),
    setGoalProgress: (event) => calls.push(['goal', event.current]),
    showGoalReached: (event) => calls.push(['reached', event.goal_id]),
    updatePrediction: (event) => calls.push([event.kind, event.prediction_id]),
  };
  const base = { stream_id: 'stream-1' };
  const events = [
    { ...base, kind: 'chat', body: 'hi' },
    { ...base, kind: 'gift', gift_id: 'rose' },
    { ...base, kind: 'viewers', count: 7 },
    { ...base, kind: 'status', status: 'live' },
    { ...base, kind: 'hype_train', level: 2 },
    { ...base, kind: 'raid', target_stream_id: 'stream-2' },
    { ...base, kind: 'points_redeemed', reward_id: 'reward-1' },
    { ...base, kind: 'goal_progress', current: 8 },
    { ...base, kind: 'goal_reached', goal_id: 'goal-1' },
    { ...base, kind: 'prediction_opened', prediction_id: 'prediction-1' },
    { ...base, kind: 'prediction_locked', prediction_id: 'prediction-1' },
    { ...base, kind: 'prediction_resolved', prediction_id: 'prediction-1' },
  ];
  for (const event of events) assert.equal(dispatchStreamEvent(controller, event), true);
  assert.deepEqual(calls.map(([kind]) => kind), [
    'chat', 'gift', 'viewers', 'status', 'hype', 'raid', 'points', 'goal',
    'reached', 'prediction_opened', 'prediction_locked', 'prediction_resolved',
  ]);
  assert.equal(dispatchStreamEvent(controller, { ...base, kind: 'future_event' }), false);
});

test('message activity reducers deduplicate at-least-once events', () => {
  assert.deepEqual(mergeSeenReader(['a'], 'a'), ['a']);
  assert.deepEqual(mergeSeenReader(['a'], 'b'), ['a', 'b']);
  const once = mergeInteraction([], { participant: 'p1', action_id: 'approve' });
  assert.deepEqual(mergeInteraction(once, { participant: 'p1', action_id: 'approve' }), once);
  assert.equal(participantLabel(new Map([['p1', { display_name: 'Ada' }]]), 'p1'), 'Ada');
});
