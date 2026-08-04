// sfu_calls.js — browser-side SFU v2 negotiation for group calls.
//
// The server owns one WebRTC media leg per participant. Local capture tracks
// use sendonly transceivers; every remote publisher track gets a distinct
// recvonly transceiver. A revision describes publisher topology while
// session_generation identifies the current server-side peer created by the
// latest SDP offer. Keeping those two counters separate prevents stale ICE or
// subscriptions from mutating a replacement media session.

export const SFU_CAPABILITY = 'call_sfu_v2';
export const DEFAULT_INITIAL_RECEIVE_SLOTS_PER_KIND = 7;
const MEDIA_KINDS = new Set(['audio', 'video']);
const MAX_SUBSCRIPTIONS = 64;
const INITIAL_RECEIVE_KINDS = ['audio', 'video'];

function asKind(value) {
  const kind = String(value || '').toLowerCase();
  return MEDIA_KINDS.has(kind) ? kind : null;
}

function sourceKey(publisher, pubMid) {
  return `${publisher}\u0000${pubMid}`;
}

/** Return a bounded, deterministic list of valid tracks published by others. */
export function remotePublishedTracks(publishers, selfId) {
  const result = [];
  const sourceSeen = new Set();
  const rows = Array.isArray(publishers) ? publishers.slice() : [];
  rows.sort((a, b) => String(a?.participant || '').localeCompare(String(b?.participant || '')));
  for (const publisher of rows) {
    const participant = String(publisher?.participant || '');
    if (!participant || participant === String(selfId || '')) continue;
    const tracks = Array.isArray(publisher?.tracks) ? publisher.tracks.slice() : [];
    tracks.sort((a, b) => String(a?.mid || '').localeCompare(String(b?.mid || '')));
    for (const track of tracks) {
      const pubMid = String(track?.mid || '');
      const mediaKind = asKind(track?.media_kind);
      const key = sourceKey(participant, pubMid);
      if (!pubMid || !mediaKind || sourceSeen.has(key)) continue;
      sourceSeen.add(key);
      result.push({ publisher: participant, pub_mid: pubMid, media_kind: mediaKind });
      if (result.length >= MAX_SUBSCRIPTIONS) return result;
    }
  }
  return result;
}

/**
 * Match remote publisher tracks to negotiated recvonly slots.
 *
 * Existing mappings are retained whenever possible so an unrelated participant
 * joining does not reshuffle every receiver. The result deliberately includes
 * `media_kind` for local rendering; callers strip it from the wire request.
 */
export function planSfuSubscriptions(publishers, selfId, slots, previous = []) {
  const sources = remotePublishedTracks(publishers, selfId);
  const validSlots = (Array.isArray(slots) ? slots : [])
    .map((slot) => ({
      mid: String(slot?.mid || ''),
      media_kind: asKind(slot?.media_kind),
      transceiver: slot?.transceiver,
    }))
    .filter((slot) => slot.mid && slot.media_kind);
  const slotByMid = new Map(validSlots.map((slot) => [slot.mid, slot]));
  const sourceByKey = new Map(sources.map((source) => [
    sourceKey(source.publisher, source.pub_mid),
    source,
  ]));
  const usedSlots = new Set();
  const assignedSources = new Set();
  const planned = [];

  for (const old of Array.isArray(previous) ? previous : []) {
    const key = sourceKey(String(old?.publisher || ''), String(old?.pub_mid || ''));
    const source = sourceByKey.get(key);
    const slot = slotByMid.get(String(old?.out_mid || ''));
    if (
      !source
      || !slot
      || source.media_kind !== slot.media_kind
      || usedSlots.has(slot.mid)
      || assignedSources.has(key)
    ) continue;
    planned.push({ ...source, out_mid: slot.mid, transceiver: slot.transceiver });
    usedSlots.add(slot.mid);
    assignedSources.add(key);
  }

  for (const source of sources) {
    const key = sourceKey(source.publisher, source.pub_mid);
    if (assignedSources.has(key)) continue;
    const slot = validSlots.find((candidate) => (
      candidate.media_kind === source.media_kind && !usedSlots.has(candidate.mid)
    ));
    if (!slot) continue;
    planned.push({ ...source, out_mid: slot.mid, transceiver: slot.transceiver });
    usedSlots.add(slot.mid);
    assignedSources.add(key);
  }

  const missing = { audio: 0, video: 0 };
  for (const source of sources) {
    if (!assignedSources.has(sourceKey(source.publisher, source.pub_mid))) {
      missing[source.media_kind] += 1;
    }
  }
  return { subscriptions: planned, missing };
}

function receiveSlots(pc) {
  return pc.getTransceivers()
    .filter((transceiver) => transceiver.direction === 'recvonly')
    .map((transceiver) => ({
      mid: transceiver.mid == null ? '' : String(transceiver.mid),
      media_kind: asKind(transceiver.receiver?.track?.kind),
      transceiver,
    }))
    .filter((slot) => slot.media_kind);
}

function wireSubscriptions(subscriptions) {
  return subscriptions.map(({ publisher, pub_mid: pubMid, out_mid: outMid }) => ({
    publisher,
    pub_mid: pubMid,
    out_mid: outMid,
  }));
}

/**
 * Stateful signaling controller. DOM rendering stays in calls.js; dependencies
 * are injected so the topology planner remains testable without browser globals.
 */
export class SfuGroupController {
  constructor({
    ws,
    getCall,
    getSelfId,
    rtcConfig,
    onSubscriptions = () => {},
    onConnectionState = () => {},
    onError = () => {},
    peerConnectionFactory = (config) => new RTCPeerConnection(config),
    initialReceiveSlotsPerKind = DEFAULT_INITIAL_RECEIVE_SLOTS_PER_KIND,
  }) {
    this.ws = ws;
    this.getCall = getCall;
    this.getSelfId = getSelfId;
    this.rtcConfig = rtcConfig;
    this.onSubscriptions = onSubscriptions;
    this.onConnectionState = onConnectionState;
    this.onError = onError;
    this.peerConnectionFactory = peerConnectionFactory;
    this.initialReceiveSlotsPerKind = Number.isSafeInteger(initialReceiveSlotsPerKind)
      ? Math.min(
        Math.max(initialReceiveSlotsPerKind, 0),
        Math.floor(MAX_SUBSCRIPTIONS / INITIAL_RECEIVE_KINDS.length),
      )
      : DEFAULT_INITIAL_RECEIVE_SLOTS_PER_KIND;
    this.pc = null;
    this.callId = null;
    this.roomId = null;
    this.sessionGeneration = null;
    this.desiredTopology = null;
    this.subscriptions = [];
    this.pendingCandidates = [];
    this.offerInFlight = false;
    this.offerQueued = false;
    this.closed = false;
    this.localSenders = new Map();
  }

  start() {
    const call = this.getCall();
    if (!call?.id || this.pc) return;
    this.closed = false;
    this.callId = call.id;
    this.roomId = call.roomId;
    const pc = this.peerConnectionFactory(this.rtcConfig());
    this.pc = pc;
    for (const track of call.localStream?.getTracks?.() || []) {
      const transceiver = pc.addTransceiver(track, {
        direction: 'sendonly',
        streams: call.localStream ? [call.localStream] : [],
      });
      this.localSenders.set(track.kind, transceiver);
    }
    // Firefox cannot reliably activate a recvonly m-line added only after the
    // first DTLS session is established and the server replaces its ICE agent.
    // Reserving the default eight-participant call's seven remote audio/video
    // pairs in the initial offer avoids that path while retaining later
    // expansion for larger SFU calls.
    for (const kind of INITIAL_RECEIVE_KINDS) {
      for (let i = 0; i < this.initialReceiveSlotsPerKind; i += 1) {
        pc.addTransceiver(kind, { direction: 'recvonly' });
      }
    }
    pc.addEventListener('icecandidate', (event) => {
      if (!event.candidate || this.closed) return;
      const candidate = typeof event.candidate.toJSON === 'function'
        ? event.candidate.toJSON()
        : event.candidate;
      if (this.sessionGeneration == null || this.offerInFlight) {
        this.pendingCandidates.push(candidate);
      } else {
        this.#sendIce(candidate);
      }
    });
    pc.addEventListener('connectionstatechange', () => {
      this.onConnectionState(pc.connectionState);
    });
    // All topology-driven additions call requestOffer explicitly. This listener
    // catches browser-generated renegotiation for local direction/track changes.
    pc.addEventListener('negotiationneeded', () => {
      if (this.callId && !this.offerInFlight) this.requestOffer();
    });
    this.requestOffer();
  }

  async requestOffer() {
    if (this.closed || !this.pc || !this.callId) return;
    if (this.offerInFlight || this.pc.signalingState !== 'stable') {
      this.offerQueued = true;
      return;
    }
    this.offerInFlight = true;
    this.offerQueued = false;
    const replacesServerSession = this.sessionGeneration != null;
    // Candidates gathered for an older server media generation must never be
    // replayed into the peer created for this offer.
    this.sessionGeneration = null;
    this.pendingCandidates = [];
    try {
      // The server creates a fresh ICE agent for every SFU offer. Firefox
      // rejects an answer with new ICE credentials unless the offer explicitly
      // requested an ICE restart; Chromium currently accepts that mismatch.
      const offer = replacesServerSession
        ? await this.pc.createOffer({ iceRestart: true })
        : await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      const sdp = this.pc.localDescription?.sdp || offer.sdp;
      if (!this.ws.callSfuOffer(this.callId, this.roomId, sdp)) {
        throw new Error('SFU offer was not accepted by the WebSocket');
      }
    } catch (error) {
      this.offerInFlight = false;
      this.onError(error);
    }
  }

  async handleAnswer(frame) {
    if (!this.#matches(frame) || !this.pc) return;
    try {
      await this.pc.setRemoteDescription({ type: 'answer', sdp: frame.sdp });
      this.sessionGeneration = Number(frame.session_generation);
      if (!Number.isSafeInteger(this.sessionGeneration) || this.sessionGeneration < 0) {
        throw new Error('invalid SFU session generation');
      }
      this.offerInFlight = false;
      for (const candidate of this.pendingCandidates.splice(0)) this.#sendIce(candidate);
      this.#adoptTopology(frame);
      const added = this.#ensureReceiveSlots();
      if (added || this.offerQueued) {
        this.offerQueued = false;
        await this.requestOffer();
      } else {
        this.#subscribe();
      }
    } catch (error) {
      this.offerInFlight = false;
      this.onError(error);
    }
  }

  async handleTopology(frame) {
    if (!this.#matches(frame) || !this.pc) return;
    const incomingRevision = Number(frame.revision);
    const currentRevision = Number(this.desiredTopology?.revision ?? -1);
    if (!Number.isSafeInteger(incomingRevision) || incomingRevision < currentRevision) return;
    this.#adoptTopology(frame);
    const added = this.#ensureReceiveSlots();
    if (added) await this.requestOffer();
    else if (!this.offerInFlight) this.#subscribe();
  }

  handleSubscribed(frame) {
    if (!this.#matches(frame)) return;
    if (Number(frame.session_generation) !== this.sessionGeneration) return;
    // The assignment is rendered optimistically when sent. This ACK is still
    // consumed so stale generations cannot be mistaken for current success.
  }

  async useScreenTrack(track) {
    if (!this.pc || !track) return;
    let transceiver = this.localSenders.get('video');
    if (!transceiver) {
      transceiver = this.pc.addTransceiver(track, { direction: 'sendonly' });
      this.localSenders.set('video', transceiver);
      await this.requestOffer();
      return;
    }
    if (transceiver.direction !== 'sendonly') {
      transceiver.direction = 'sendonly';
      await transceiver.sender.replaceTrack(track);
      await this.requestOffer();
      return;
    }
    await transceiver.sender.replaceTrack(track);
  }

  async restoreCameraTrack(track) {
    const transceiver = this.localSenders.get('video');
    if (!transceiver) return;
    await transceiver.sender.replaceTrack(track || null);
    if (!track && transceiver.direction !== 'inactive') {
      transceiver.direction = 'inactive';
      await this.requestOffer();
    } else if (track && transceiver.direction !== 'sendonly') {
      transceiver.direction = 'sendonly';
      await this.requestOffer();
    }
  }

  close() {
    this.closed = true;
    try { this.pc?.close(); } catch { /* already closed */ }
    this.pc = null;
    this.callId = null;
    this.roomId = null;
    this.sessionGeneration = null;
    this.desiredTopology = null;
    this.subscriptions = [];
    this.pendingCandidates = [];
    this.offerInFlight = false;
    this.offerQueued = false;
    this.localSenders.clear();
    this.onSubscriptions([]);
  }

  #matches(frame) {
    return !this.closed && frame?.call_id === this.callId;
  }

  #adoptTopology(frame) {
    const revision = Number(frame.revision);
    if (!Number.isSafeInteger(revision) || revision < 0) return;
    const existing = Number(this.desiredTopology?.revision ?? -1);
    if (revision < existing) return;
    this.desiredTopology = {
      revision,
      publishers: Array.isArray(frame.publishers) ? frame.publishers : [],
      required_recv_slots: Number(frame.required_recv_slots) || 0,
    };
  }

  #ensureReceiveSlots() {
    if (!this.pc || !this.desiredTopology) return false;
    const plan = planSfuSubscriptions(
      this.desiredTopology.publishers,
      this.getSelfId(),
      receiveSlots(this.pc),
      this.subscriptions,
    );
    let added = false;
    for (const kind of ['audio', 'video']) {
      for (let i = 0; i < plan.missing[kind]; i += 1) {
        this.pc.addTransceiver(kind, { direction: 'recvonly' });
        added = true;
      }
    }
    return added;
  }

  #subscribe() {
    if (
      !this.pc
      || !this.desiredTopology
      || this.sessionGeneration == null
      || this.offerInFlight
    ) return;
    const plan = planSfuSubscriptions(
      this.desiredTopology.publishers,
      this.getSelfId(),
      receiveSlots(this.pc),
      this.subscriptions,
    );
    if (plan.missing.audio || plan.missing.video) {
      this.#ensureReceiveSlots();
      this.requestOffer();
      return;
    }
    this.subscriptions = plan.subscriptions;
    this.onSubscriptions(this.subscriptions);
    this.ws.callSfuSubscribe(
      this.callId,
      this.roomId,
      this.sessionGeneration,
      this.desiredTopology.revision,
      wireSubscriptions(this.subscriptions),
    );
  }

  #sendIce(candidate) {
    if (this.sessionGeneration == null || this.closed) return;
    this.ws.callSfuIce(
      this.callId,
      this.roomId,
      this.sessionGeneration,
      candidate,
    );
  }
}
