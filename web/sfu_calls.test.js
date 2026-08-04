import assert from 'node:assert/strict';
import test from 'node:test';

import {
  DEFAULT_INITIAL_RECEIVE_SLOTS_PER_KIND,
  planSfuSubscriptions,
  remotePublishedTracks,
  SFU_CAPABILITY,
  SfuGroupController,
} from './sfu_calls.js';

test('SFU v2 capability is explicit', () => {
  assert.equal(SFU_CAPABILITY, 'call_sfu_v2');
});

test('remote tracks exclude self, reject malformed rows, and sort deterministically', () => {
  const tracks = remotePublishedTracks([
    {
      participant: 'peer-b',
      tracks: [
        { mid: 'v', media_kind: 'video' },
        { mid: 'a', media_kind: 'audio' },
      ],
    },
    { participant: 'me', tracks: [{ mid: 'mine', media_kind: 'audio' }] },
    { participant: 'peer-a', tracks: [{ mid: '', media_kind: 'audio' }] },
  ], 'me');
  assert.deepEqual(tracks, [
    { publisher: 'peer-b', pub_mid: 'a', media_kind: 'audio' },
    { publisher: 'peer-b', pub_mid: 'v', media_kind: 'video' },
  ]);
});

test('subscription plan is publisher scoped when peers reuse the same MID', () => {
  const publishers = [
    { participant: 'p1', tracks: [{ mid: '0', media_kind: 'audio' }] },
    { participant: 'p2', tracks: [{ mid: '0', media_kind: 'audio' }] },
  ];
  const slots = [
    { mid: 'recv-a-1', media_kind: 'audio' },
    { mid: 'recv-a-2', media_kind: 'audio' },
  ];
  const plan = planSfuSubscriptions(publishers, 'me', slots);
  assert.deepEqual(plan.subscriptions.map((row) => ({
    publisher: row.publisher,
    pub_mid: row.pub_mid,
    media_kind: row.media_kind,
    out_mid: row.out_mid,
  })), [
    { publisher: 'p1', pub_mid: '0', media_kind: 'audio', out_mid: 'recv-a-1' },
    { publisher: 'p2', pub_mid: '0', media_kind: 'audio', out_mid: 'recv-a-2' },
  ]);
  assert.deepEqual(plan.missing, { audio: 0, video: 0 });
});

test('subscription plan never crosses media kinds and reports exact missing slots', () => {
  const publishers = [
    {
      participant: 'peer',
      tracks: [
        { mid: 'a', media_kind: 'audio' },
        { mid: 'v', media_kind: 'video' },
      ],
    },
  ];
  const plan = planSfuSubscriptions(
    publishers,
    'me',
    [{ mid: 'recv-video', media_kind: 'video' }],
  );
  assert.equal(plan.subscriptions.length, 1);
  assert.equal(plan.subscriptions[0].pub_mid, 'v');
  assert.deepEqual(plan.missing, { audio: 1, video: 0 });
});

test('existing valid routes stay stable when a publisher joins', () => {
  const previous = [
    {
      publisher: 'p2',
      pub_mid: 'a',
      media_kind: 'audio',
      out_mid: 'recv-a-2',
    },
  ];
  const plan = planSfuSubscriptions([
    { participant: 'p1', tracks: [{ mid: 'a', media_kind: 'audio' }] },
    { participant: 'p2', tracks: [{ mid: 'a', media_kind: 'audio' }] },
  ], 'me', [
    { mid: 'recv-a-1', media_kind: 'audio' },
    { mid: 'recv-a-2', media_kind: 'audio' },
  ], previous);
  const p2 = plan.subscriptions.find((row) => row.publisher === 'p2');
  assert.equal(p2.out_mid, 'recv-a-2');
});

class FakePeerConnection {
  constructor() {
    this.signalingState = 'stable';
    this.connectionState = 'new';
    this.localDescription = null;
    this.transceivers = [];
    this.listeners = new Map();
    this.offerCount = 0;
    this.offerOptions = [];
  }

  addTransceiver(trackOrKind, options = {}) {
    const kind = typeof trackOrKind === 'string' ? trackOrKind : trackOrKind.kind;
    const transceiver = {
      mid: null,
      direction: options.direction || 'sendrecv',
      sender: {
        track: typeof trackOrKind === 'string' ? null : trackOrKind,
        async replaceTrack(track) { this.track = track; },
      },
      receiver: { track: { kind, readyState: 'live' } },
    };
    this.transceivers.push(transceiver);
    return transceiver;
  }

  getTransceivers() { return this.transceivers; }

  addEventListener(name, handler) {
    if (!this.listeners.has(name)) this.listeners.set(name, []);
    this.listeners.get(name).push(handler);
  }

  emit(name, event) {
    for (const handler of this.listeners.get(name) || []) handler(event);
  }

  async createOffer(options) {
    this.offerCount += 1;
    this.offerOptions.push(options);
    return { type: 'offer', sdp: `v=0\r\no=${this.offerCount}\r\n` };
  }

  async setLocalDescription(description) {
    this.localDescription = description;
    this.signalingState = 'have-local-offer';
    this.transceivers.forEach((transceiver, index) => {
      transceiver.mid = String(index);
    });
  }

  async setRemoteDescription() {
    this.signalingState = 'stable';
  }

  close() {
    this.connectionState = 'closed';
    this.signalingState = 'closed';
  }
}

test('default Firefox receive pool covers seven remote publishers without re-offer', async () => {
  const pc = new FakePeerConnection();
  const sent = [];
  const ws = {
    callSfuOffer(callId, roomId, sdp) {
      sent.push({ type: 'offer', callId, roomId, sdp });
      return true;
    },
    callSfuIce() { return true; },
    callSfuSubscribe(callId, roomId, generation, revision, tracks) {
      sent.push({
        type: 'subscribe',
        callId,
        roomId,
        generation,
        revision,
        tracks,
      });
      return true;
    },
  };
  const controller = new SfuGroupController({
    ws,
    getCall: () => ({
      id: 'call-pool',
      roomId: 'room-pool',
      localStream: {
        getTracks: () => [{ kind: 'audio' }, { kind: 'video' }],
      },
    }),
    getSelfId: () => 'me',
    rtcConfig: () => ({}),
    peerConnectionFactory: () => pc,
  });

  controller.start();
  await new Promise((resolve) => globalThis.setTimeout(resolve, 0));
  assert.equal(DEFAULT_INITIAL_RECEIVE_SLOTS_PER_KIND, 7);
  assert.equal(pc.offerCount, 1);
  assert.equal(pc.offerOptions[0], undefined);
  const receivers = pc.transceivers.filter(
    (transceiver) => transceiver.direction === 'recvonly',
  );
  assert.equal(receivers.length, 14);
  assert.equal(
    receivers.filter((transceiver) => transceiver.receiver.track.kind === 'audio').length,
    7,
  );
  assert.equal(
    receivers.filter((transceiver) => transceiver.receiver.track.kind === 'video').length,
    7,
  );

  const publishers = [
    {
      participant: 'me',
      tracks: [
        { mid: '0', media_kind: 'audio' },
        { mid: '1', media_kind: 'video' },
      ],
    },
    ...Array.from({ length: 7 }, (_, index) => ({
      participant: `peer-${index}`,
      tracks: [
        { mid: '0', media_kind: 'audio' },
        { mid: '1', media_kind: 'video' },
      ],
    })),
  ];
  await controller.handleAnswer({
    call_id: 'call-pool',
    revision: 8,
    publishers,
    required_recv_slots: 14,
    sdp: 'v=0\r\n',
    session_generation: 8,
  });
  assert.equal(pc.offerCount, 1, 'topology inside the initial pool must not re-offer');
  const subscribe = sent.find((row) => row.type === 'subscribe');
  assert.equal(subscribe.tracks.length, 14);
});

test('controller buffers ICE and restarts it only when receive pool expands', async () => {
  const pc = new FakePeerConnection();
  const sent = [];
  const rendered = [];
  const ws = {
    callSfuOffer(callId, roomId, sdp) {
      sent.push({ type: 'offer', callId, roomId, sdp });
      return true;
    },
    callSfuIce(callId, roomId, generation, candidate) {
      sent.push({ type: 'ice', callId, roomId, generation, candidate });
      return true;
    },
    callSfuSubscribe(callId, roomId, generation, revision, tracks) {
      sent.push({ type: 'subscribe', callId, roomId, generation, revision, tracks });
      return true;
    },
  };
  const call = {
    id: 'call-1',
    roomId: 'room-1',
    localStream: {
      getTracks: () => [{ kind: 'audio' }],
    },
  };
  const controller = new SfuGroupController({
    ws,
    getCall: () => call,
    getSelfId: () => 'me',
    rtcConfig: () => ({}),
    peerConnectionFactory: () => pc,
    onSubscriptions: (rows) => rendered.push(rows),
    initialReceiveSlotsPerKind: 1,
  });

  controller.start();
  await new Promise((resolve) => globalThis.setTimeout(resolve, 0));
  assert.equal(sent.filter((row) => row.type === 'offer').length, 1);
  assert.equal(pc.offerOptions[0], undefined);
  assert.equal(pc.transceivers[0].direction, 'sendonly');
  assert.deepEqual(
    pc.transceivers.slice(1).map((transceiver) => [
      transceiver.direction,
      transceiver.receiver.track.kind,
    ]),
    [
      ['recvonly', 'audio'],
      ['recvonly', 'video'],
    ],
  );

  pc.emit('icecandidate', {
    candidate: { toJSON: () => ({ candidate: 'candidate:first' }) },
  });
  assert.equal(sent.filter((row) => row.type === 'ice').length, 0);

  const topology = {
    call_id: 'call-1',
    revision: 4,
    publishers: [
      { participant: 'me', tracks: [{ mid: '0', media_kind: 'audio' }] },
      { participant: 'peer', tracks: [{ mid: '0', media_kind: 'audio' }] },
      { participant: 'peer-two', tracks: [{ mid: '0', media_kind: 'audio' }] },
    ],
    required_recv_slots: 2,
  };
  await controller.handleAnswer({
    ...topology,
    sdp: 'v=0\r\n',
    session_generation: 9,
  });
  assert.equal(sent.find((row) => row.type === 'ice').generation, 9);
  assert.equal(pc.transceivers[3].direction, 'recvonly');
  assert.equal(sent.filter((row) => row.type === 'offer').length, 2);
  assert.deepEqual(
    pc.offerOptions[1],
    { iceRestart: true },
    'Firefox requires an explicit ICE restart when the server session changes',
  );

  pc.emit('icecandidate', {
    candidate: { toJSON: () => ({ candidate: 'candidate:second' }) },
  });
  await controller.handleAnswer({
    ...topology,
    sdp: 'v=0\r\n',
    session_generation: 10,
  });
  const secondIce = sent.filter((row) => row.type === 'ice').at(-1);
  assert.equal(secondIce.generation, 10);
  const subscribe = sent.find((row) => row.type === 'subscribe');
  assert.deepEqual(subscribe, {
    type: 'subscribe',
    callId: 'call-1',
    roomId: 'room-1',
    generation: 10,
    revision: 4,
    tracks: [
      { publisher: 'peer', pub_mid: '0', out_mid: '1' },
      { publisher: 'peer-two', pub_mid: '0', out_mid: '3' },
    ],
  });
  assert.equal(rendered.at(-1)[0].transceiver, pc.transceivers[1]);
});
