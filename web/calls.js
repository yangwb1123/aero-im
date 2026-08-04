// calls.js — real-time call domain (extracted from app.js).
//
// Three cohesive sub-domains that only reference each other plus the shared
// spine (state / ws / els / cssEscape) and render's `toast`:
//   • 1:1 call (WebRTC P2P)
//   • live captions (P3 实时字幕翻译)
//   • group call (P6 full-mesh)
//
// Public surface (imported by app.js):
//   • handleCall(event)   — routed from ws `msg:call`
//   • wireCallControls()  — attach the call/gcall toolbar button listeners
//
// Everything else stays module-private; the original wiring and behaviour are
// preserved verbatim.

import { state, ws, els, cssEscape } from './context.js';
import { toast } from './render.js';
import { browserRtcConfig } from './rtc_config.js';
import { SFU_CAPABILITY, SfuGroupController } from './sfu_calls.js';

let sfuGroup = null;

// ---------- 1:1 call (WebRTC P2P) ----------
// Attach the call/gcall control-button listeners. Called once at bootstrap so
// the DOM-event wiring lives with the call logic instead of in app.js.
export function wireCallControls() {
  els.btnCallAudio.addEventListener('click', () => startCall('audio'));
  els.btnCallVideo.addEventListener('click', () => startCall('video'));
  els.btnCallGroup.addEventListener('click', () => startGroupCall('video'));
  els.gcallLeave.addEventListener('click', () => leaveGroupCall());
  els.gcallMute.addEventListener('click', () => gcallToggleTrack('audio'));
  els.gcallCam.addEventListener('click', () => gcallToggleTrack('video'));
  els.gcallShare.addEventListener('click', () => gcallToggleScreenShare());
  els.callEnd.addEventListener('click', () => endCall('hangup'));
  els.callMute.addEventListener('click', () => toggleTrack('audio'));
  els.callCam.addEventListener('click', () => toggleTrack('video'));
  els.callShare.addEventListener('click', () => toggleScreenShare());
  els.callCc.addEventListener('click', () => toggleCaptions());
  els.callCcLang.addEventListener('change', () => { if (state.call) state.call.targetLang = els.callCcLang.value || null; });
  ws.on('msg:call_sfu_answer', (frame) => sfuGroup?.handleAnswer(frame));
  ws.on('msg:call_sfu_renegotiate', (frame) => sfuGroup?.handleTopology(frame));
  ws.on('msg:call_sfu_subscribed', (frame) => sfuGroup?.handleSubscribed(frame));
  ws.on('msg:welcome', () => {
    const g = state.gcall;
    if (!g || g.mode !== 'sfu' || !g.id) return;
    if (!ws.supports(SFU_CAPABILITY)) {
      // A rolling downgrade can reconnect this tab to an older gateway. Keep
      // the call usable through the legacy mesh protocol instead of sending
      // frames that gateway cannot decode.
      sfuGroup?.close();
      g.mode = 'mesh';
      ws.callJoin(g.roomId, g.kind, g.id);
      return;
    }
    // The server owns the media peer and removes it with the old WS session.
    // Rejoin idempotently after reconnect so a fresh roster starts a new
    // generation instead of leaving the browser stuck on a dead peer.
    sfuGroup?.close();
    ws.callJoin(g.roomId, g.kind, g.id);
  });
}

function toggleTrack(kind) {
  const s = state.call?.localStream;
  if (!s) return;
  for (const t of s.getTracks()) if (t.kind === kind) t.enabled = !t.enabled;
}

async function startCall(kind) {
  if (!state.currentRoomId) { toast('请选择房间', 'error'); return; }
  if (state.call) { toast('已在通话中', 'error'); return; }
  try {
    const localStream = await navigator.mediaDevices.getUserMedia({
      audio: true, video: kind === 'video',
    });
    state.call = {
      id: null, roomId: state.currentRoomId, kind,
      pc: null, localStream, remoteStream: null,
      screenTrack: null, camTrack: null, negotiationReady: false,
    };
    els.callLocal.srcObject = localStream;
    els.callOverlay.hidden = false;
    const pc = makePeer();
    state.call.pc = pc;
    for (const t of localStream.getTracks()) pc.addTrack(t, localStream);
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    // server creates the call session + fans out invite via NATS.
    ws.callInvite(state.currentRoomId, kind, offer.sdp);
    // From now on, any track change should auto-renegotiate (screen share etc).
    state.call.negotiationReady = true;
  } catch (err) {
    toast(`通话失败:${err.message}`, 'error');
    endCall('error');
  }
}

function makePeer() {
  const pc = new RTCPeerConnection(rtcIceConfig());
  pc.addEventListener('icecandidate', (e) => {
    if (!e.candidate || !state.call) return;
    ws.callIce(state.call.id, state.call.roomId, state.call.peer, e.candidate.toJSON());
  });
  pc.addEventListener('track', (e) => {
    if (!state.call) return;
    state.call.remoteStream = e.streams[0];
    els.callRemote.srcObject = state.call.remoteStream;
  });
  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') endCall(pc.connectionState);
  });
  // Renegotiation: adding/removing a track after the initial offer (e.g. starting
  // or stopping screen share) fires `negotiationneeded`. The very first offer is
  // sent by hand in startCall/handleCall before `peer` is known, so we gate on a
  // ready flag + a known peer and only auto-renegotiate for later changes.
  pc.addEventListener('negotiationneeded', () => callRenegotiate());
  return pc;
}

// Send a fresh offer over the existing 1:1 signaling path after a media change
// (screen share start/stop). Reuses the `call_offer` frame — handleCall routes a
// matching `offer` op back to onCallOffer, which answers. Best-effort: a transient
// glare/state error is logged, not fatal — the audio/video call keeps running.
async function callRenegotiate() {
  const c = state.call;
  if (!c || !c.pc || !c.peer || !c.negotiationReady) return;
  try {
    const offer = await c.pc.createOffer();
    if (c.pc.signalingState !== 'stable') return; // a parallel negotiation is in flight
    await c.pc.setLocalDescription(offer);
    ws.callOffer(c.id, c.roomId, c.peer, offer.sdp);
  } catch (err) { console.warn('[call renegotiate]', err); }
}

// Incoming re-offer on the active 1:1 call (peer added/removed a screen track).
// Answers it on the existing pc — the new screen track surfaces via the normal
// `track` handler below. Distinct from the initial invite (which also creates the
// localStream / overlay); this only re-answers an established call.
async function onCallOffer(event) {
  const c = state.call;
  if (!c || c.id !== event.call_id || !c.pc) return;
  try {
    await c.pc.setRemoteDescription({ type: 'offer', sdp: event.sdp });
    const ans = await c.pc.createAnswer();
    await c.pc.setLocalDescription(ans);
    ws.callAnswer(c.id, c.roomId, event.from, ans.sdp);
  } catch (err) { console.warn('[call re-offer]', err); }
}

export async function handleCall(event) {
  const op = event?.op;
  // ---- group-call (P6 mesh) routing ----
  if (op === 'roster') return gcallOnRoster(event);
  if (op === 'join') return gcallOnJoin(event);
  if (op === 'leave') return gcallOnLeave(event);
  if (op === 'offer') {
    // A re-offer on the active 1:1 call (renegotiation, e.g. screen share)
    // belongs to the P2P handler; everything else is a group-mesh offer.
    if (state.call && event.call_id === state.call.id) return onCallOffer(event);
    return gcallOnOffer(event);
  }
  // answer/ice belonging to the active group call route to the mesh handlers
  if (
    state.gcall?.mode === 'mesh'
    && event.call_id === state.gcall.id
    && (op === 'answer' || op === 'ice')
  ) {
    return op === 'answer' ? gcallOnAnswer(event) : gcallOnIce(event);
  }
  if (op === 'invite') {
    if (event.from === state.me?.id) {
      state.call = state.call || { id: event.call_id, roomId: event.room_id };
      state.call.id = event.call_id;
      state.call.peer = (event.to || []).find((p) => p !== state.me?.id) || event.to?.[0];
      return;
    }
    if (state.call) return; // already in another call
    const callKind = event.call_kind || event.kind; // server renamed kind→call_kind on the wire
    if (!confirm(`收到 ${callKind} 通话邀请,接听?`)) {
      ws.callEnd(event.call_id, event.room_id, 'declined');
      return;
    }
    try {
      const localStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: callKind === 'video' });
      state.call = {
        id: event.call_id, roomId: event.room_id, kind: callKind,
        pc: null, localStream, remoteStream: null, peer: event.from,
        screenTrack: null, camTrack: null, negotiationReady: false,
      };
      els.callLocal.srcObject = localStream;
      els.callOverlay.hidden = false;
      const pc = makePeer();
      state.call.pc = pc;
      for (const t of localStream.getTracks()) pc.addTrack(t, localStream);
      await pc.setRemoteDescription({ type: 'offer', sdp: event.sdp });
      const ans = await pc.createAnswer();
      await pc.setLocalDescription(ans);
      ws.callAnswer(event.call_id, event.room_id, event.from, ans.sdp);
      // Established → later track changes (screen share) auto-renegotiate.
      state.call.negotiationReady = true;
    } catch (err) {
      toast(`接听失败:${err.message}`, 'error');
      ws.callEnd(event.call_id, event.room_id, 'gum_failed');
      endCall('error');
    }
  } else if (op === 'answer') {
    if (!state.call || state.call.id !== event.call_id) return;
    state.call.peer = event.from;
    try { await state.call.pc.setRemoteDescription({ type: 'answer', sdp: event.sdp }); }
    catch (err) { toast(`SDP 失败:${err.message}`, 'error'); }
  } else if (op === 'ice') {
    if (!state.call || state.call.id !== event.call_id) return;
    try { await state.call.pc.addIceCandidate(event.candidate); }
    catch (err) { console.warn('addIceCandidate', err); }
  } else if (op === 'end') {
    if (state.call && state.call.id === event.call_id) endCall('remote_end');
  } else if (op === 'caption') {
    if (!state.call || state.call.id !== event.call_id) return;
    if (event.from === state.me?.id) return; // our own captions render locally
    const name = state.participants.get(event.from)?.display_name || '对方';
    renderCaption(event.from, name, event.text, event.translated, event.is_final);
  }
}

function endCall(reason) {
  const c = state.call;
  if (!c) { els.callOverlay.hidden = true; return; }
  stopRecognition();
  try { c.screenTrack?.stop(); } catch { /* best-effort cleanup */ }
  try { c.pc?.close(); } catch { /* best-effort cleanup */ }
  try { c.localStream?.getTracks().forEach((t) => t.stop()); } catch { /* best-effort cleanup */ }
  if (c.id && c.roomId) {
    try { ws.callEnd(c.id, c.roomId, reason || 'hangup'); } catch { /* socket may be gone */ }
  }
  els.callLocal.srcObject = null;
  els.callRemote.srcObject = null;
  els.callShare?.classList.remove('active');
  els.callCaptions.replaceChildren();
  els.callCaptions.hidden = true;
  els.callCc.classList.remove('active');
  els.callOverlay.hidden = true;
  state.call = null;
}

// ---------- screen share (1:1) ----------
// Conservative + additive: never tears down the audio/video call. Swaps the
// outgoing video on the *existing* video sender to the screen track (or adds a
// sender if the call started audio-only). `replaceTrack` is the lightest swap —
// when it can't be done without a new m-line, addTrack + negotiationneeded
// covers it. The real screen pixels can only be confirmed by a second browser
// (E2E / staging seam); here we keep the local state & signaling self-consistent.
async function toggleScreenShare() {
  const c = state.call;
  if (!c || !c.pc) return;
  if (c.screenTrack) { stopScreenShare(); return; }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia) {
    toast('当前浏览器不支持屏幕共享', 'error');
    return;
  }
  let display;
  try {
    display = await navigator.mediaDevices.getDisplayMedia({ video: true });
  } catch (err) {
    // User cancelled the picker or permission denied — degrade gracefully.
    if (err && err.name !== 'NotAllowedError' && err.name !== 'AbortError') {
      toast(`屏幕共享失败:${err.message}`, 'error');
    }
    return;
  }
  const screenTrack = display.getVideoTracks()[0];
  if (!screenTrack) { toast('未获取到屏幕画面', 'error'); return; }
  c.screenTrack = screenTrack;
  // Remember the camera track so "stop" can restore it on the same sender.
  const sender = c.pc.getSenders().find((s) => s.track && s.track.kind === 'video');
  c.camTrack = sender ? sender.track : null;
  try {
    if (sender) {
      // replaceTrack swaps media without a new m-line (no renegotiation needed),
      // but we still flag ready so any browser that *does* renegotiate is handled.
      await sender.replaceTrack(screenTrack);
    } else {
      // Audio-only call: add a video sender → fires negotiationneeded → re-offer.
      c.pc.addTrack(screenTrack, c.localStream);
    }
  } catch (err) {
    toast(`屏幕共享失败:${err.message}`, 'error');
    try { screenTrack.stop(); } catch { /* track may already be ended */ }
    c.screenTrack = null; c.camTrack = null;
    return;
  }
  // Show what we're sharing locally; mark the button active.
  els.callLocal.srcObject = display;
  els.callShare?.classList.add('active');
  // The user can stop sharing from the browser's own "Stop sharing" UI.
  screenTrack.addEventListener('ended', () => { if (state.call?.screenTrack === screenTrack) stopScreenShare(); });
}

// Stop screen share: restore the camera (or remove the added sender), surface the
// local camera again, and let renegotiation settle the change with the peer.
function stopScreenShare() {
  const c = state.call;
  if (!c || !c.screenTrack) return;
  const screenTrack = c.screenTrack;
  c.screenTrack = null;
  try { screenTrack.stop(); } catch { /* track may already be ended */ }
  const sender = c.pc?.getSenders().find((s) => s.track === screenTrack)
    || c.pc?.getSenders().find((s) => s.track && s.track.kind === 'video');
  if (sender) {
    // Restore the camera track if we had one; otherwise drop to no outgoing video.
    sender.replaceTrack(c.camTrack || null).catch((err) => console.warn('[call unshare]', err));
  }
  c.camTrack = null;
  els.callLocal.srcObject = c.localStream;
  els.callShare?.classList.remove('active');
}

// ---------- live captions (P3 实时字幕翻译) ----------

function toggleCaptions() {
  if (!state.call) return;
  if (state.call.recog) { stopRecognition(); return; }
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SR) { toast('当前浏览器不支持语音识别(建议 Chrome/Edge)', 'error'); return; }
  const srcLang = navigator.language || 'zh-CN';
  state.call.srcLang = srcLang;
  state.call.targetLang = els.callCcLang.value || null;
  let recog;
  try { recog = new SR(); } catch { toast('字幕启动失败', 'error'); return; }
  recog.lang = srcLang;
  recog.continuous = true;
  recog.interimResults = true;
  recog.addEventListener('result', (e) => onSpeech(e));
  recog.addEventListener('error', (e) => { if (e.error !== 'no-speech') console.warn('[speech]', e.error); });
  recog.addEventListener('end', () => {
    // SpeechRecognition stops itself periodically; restart while captions are on.
    if (state.call && state.call.recog === recog) {
      try { recog.start(); } catch { /* recognition may already be active */ }
    }
  });
  try { recog.start(); } catch { toast('字幕启动失败', 'error'); return; }
  state.call.recog = recog;
  els.callCc.classList.add('active');
  els.callCaptions.hidden = false;
}

function stopRecognition() {
  const c = state.call;
  if (c?.recog) {
    const r = c.recog;
    c.recog = null; // prevent the 'end' handler from restarting
    try { r.stop(); } catch { /* recognition may already be stopped */ }
  }
  els.callCc?.classList.remove('active');
}

function onSpeech(e) {
  if (!state.call) return;
  for (let i = e.resultIndex; i < e.results.length; i++) {
    const res = e.results[i];
    const text = (res[0]?.transcript || '').trim();
    if (!text) continue;
    const isFinal = res.isFinal;
    // Render my own caption locally; broadcast (server translates final lines).
    renderCaption('me', '我', text, null, isFinal);
    ws.callCaption(state.call.id, state.call.roomId, text, state.call.srcLang, isFinal, state.call.targetLang);
  }
}

// Maintain a rolling subtitle list; one in-progress (interim) line per speaker.
function renderCaption(key, name, text, translated, isFinal) {
  const box = els.callCaptions;
  if (!box) return;
  box.hidden = false;
  state.call._capLines = state.call._capLines || new Map();
  let line = state.call._capLines.get(key);
  if (!line) {
    line = document.createElement('div');
    line.className = 'cap-line';
    const who = document.createElement('span');
    who.className = 'cap-who';
    who.textContent = `${name}: `;
    const orig = document.createElement('span');
    orig.className = 'cap-text';
    const tr = document.createElement('div');
    tr.className = 'cap-tr';
    line.appendChild(who);
    line.appendChild(orig);
    line.appendChild(tr);
    box.appendChild(line);
    line._orig = orig;
    line._tr = tr;
    state.call._capLines.set(key, line);
  }
  line._orig.textContent = text;
  if (translated) line._tr.textContent = translated;
  line.classList.toggle('interim', !isFinal);
  if (isFinal) state.call._capLines.delete(key); // next utterance starts a fresh line
  while (box.childElementCount > 5) box.removeChild(box.firstElementChild);
  box.scrollTop = box.scrollHeight;
}

// ---------- group call (P6 mesh) ----------
// Full-mesh: each participant holds one RTCPeerConnection per other participant.
// Glare-free pairing — for any pair, the peer with the smaller participant id
// creates the offer. Media is browser-native P2P; the server only relays
// signaling + tracks the roster.

function rtcIceConfig() {
  return browserRtcConfig(state.rtcConfig);
}

const gcallPrompted = new Set(); // call_ids we've already offered to join

// Start a new group call in the current room (callId === null → server creates).
function startGroupCall(kind) { return enterGroupCall(state.currentRoomId, kind, null); }

// Join an existing group call we were invited to.
function joinGroupCall(roomId, kind, callId) { return enterGroupCall(roomId, kind, callId); }

async function enterGroupCall(roomId, kind, callId) {
  if (!roomId) { toast('请选择房间', 'error'); return; }
  if (state.gcall) { toast('已在群通话中', 'error'); return; }
  if (state.call) { toast('请先结束 1:1 通话', 'error'); return; }
  try {
    const localStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: kind === 'video' });
    const mode = ws.supports(SFU_CAPABILITY) ? 'sfu' : 'mesh';
    state.gcall = {
      id: callId,
      roomId,
      kind,
      mode,
      localStream,
      peers: new Map(),
      memberGenerations: new Map(),
      screenTrack: null,
      screenStream: null,
      camTrack: localStream.getVideoTracks()[0] || null,
    };
    els.gcallGrid.replaceChildren();
    addGcallTile('me', '我', localStream, true);
    els.gcallOverlay.hidden = false;
    updateGcallCount();
    ws.callJoin(roomId, kind, callId); // null → create; else join existing → server sends roster
  } catch (err) {
    toast(`群通话失败:${err.message}`, 'error');
    leaveGroupCall();
  }
}

function promptJoinGroupCall(ev) {
  if (gcallPrompted.has(ev.call_id)) return;
  gcallPrompted.add(ev.call_id);
  const name = state.participants.get(ev.from)?.display_name || '有人';
  const kind = ev.call_kind || 'video';
  if (confirm(`${name} 发起了群通话,加入?`)) {
    joinGroupCall(ev.room_id, kind, ev.call_id);
  }
}

function gcallPeer(peerId) {
  if (!state.gcall) return null;
  let entry = state.gcall.peers.get(peerId);
  if (entry) return entry;
  const pc = new RTCPeerConnection(rtcIceConfig());
  pc.addEventListener('icecandidate', (e) => {
    if (e.candidate && state.gcall) ws.callIce(state.gcall.id, state.gcall.roomId, peerId, e.candidate.toJSON());
  });
  pc.addEventListener('track', (e) => {
    const name = state.participants.get(peerId)?.display_name || peerId.slice(0, 6);
    addGcallTile(peerId, name, e.streams[0], false);
  });
  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'closed') removeGcallPeer(peerId);
  });
  // Mesh renegotiation: when a track is added (e.g. an audio-only call starts
  // screen share, adding a video m-line) re-offer to this peer over the existing
  // `call_offer` path — gcallOnOffer answers on the same pc. Best-effort.
  pc.addEventListener('negotiationneeded', () => gcallRenegotiate(peerId));
  for (const t of state.gcall.localStream.getTracks()) pc.addTrack(t, state.gcall.localStream);
  entry = { pc };
  state.gcall.peers.set(peerId, entry);
  updateGcallCount();
  return entry;
}

async function gcallOfferTo(peerId) {
  const entry = gcallPeer(peerId);
  if (!entry) return;
  try {
    const offer = await entry.pc.createOffer();
    await entry.pc.setLocalDescription(offer);
    entry.negotiationReady = true; // later track changes may auto-renegotiate
    ws.callOffer(state.gcall.id, state.gcall.roomId, peerId, offer.sdp);
  } catch (err) { console.warn('[gcall offer]', err); }
}

// Re-offer to one mesh peer after a local media change (screen share start/stop).
// Gated on `negotiationReady` so the initial track-add during peer construction
// never fires a premature offer. Skips if a negotiation is already in flight.
async function gcallRenegotiate(peerId) {
  const entry = state.gcall?.peers.get(peerId);
  if (!entry || !entry.negotiationReady) return;
  if (entry.pc.signalingState !== 'stable') return;
  try {
    const offer = await entry.pc.createOffer();
    await entry.pc.setLocalDescription(offer);
    ws.callOffer(state.gcall.id, state.gcall.roomId, peerId, offer.sdp);
  } catch (err) { console.warn('[gcall renegotiate]', err); }
}

async function gcallOnRoster(ev) {
  if (!state.gcall) return;
  state.gcall.id = ev.call_id;
  const ownGeneration = callLegGeneration(ev);
  if (ownGeneration > 0 && state.me?.id) {
    state.gcall.memberGenerations.set(state.me.id, ownGeneration);
  }
  if (state.gcall.mode === 'sfu') {
    ensureSfuGroup().start();
    return;
  }
  for (const m of (ev.members || [])) {
    if (m === state.me?.id) continue;
    // lower id offers; otherwise wait for their offer
    if (String(state.me?.id) < String(m)) await gcallOfferTo(m);
  }
}

async function gcallOnJoin(ev) {
  const generation = callLegGeneration(ev);
  if (ev.from === state.me?.id) {
    if (state.gcall && generation > 0) {
      state.gcall.memberGenerations.set(ev.from, generation);
    }
    return;
  }
  if (!state.gcall) {
    // Invited to a group call we're not in yet — offer to join.
    promptJoinGroupCall(ev);
    return;
  }
  if (ev.call_id !== state.gcall.id) return;
  const currentGeneration = state.gcall.memberGenerations.get(ev.from) || 0;
  if (generation === 0 && currentGeneration > 0) return;
  if (generation > 0 && generation < currentGeneration) return;
  if (generation > 0) state.gcall.memberGenerations.set(ev.from, generation);
  if (state.gcall.mode === 'sfu') return;
  if (String(state.me?.id) < String(ev.from)) await gcallOfferTo(ev.from);
}

async function gcallOnOffer(ev) {
  if (!state.gcall || ev.call_id !== state.gcall.id) return;
  if (state.gcall.mode === 'sfu') return;
  if (ev.to !== state.me?.id) return;
  const entry = gcallPeer(ev.from);
  if (!entry) return;
  try {
    await entry.pc.setRemoteDescription({ type: 'offer', sdp: ev.sdp });
    const ans = await entry.pc.createAnswer();
    await entry.pc.setLocalDescription(ans);
    entry.negotiationReady = true; // established → later track changes may re-offer
    ws.callAnswer(state.gcall.id, state.gcall.roomId, ev.from, ans.sdp);
  } catch (err) { console.warn('[gcall offer-in]', err); }
}

async function gcallOnAnswer(ev) {
  const entry = state.gcall?.peers.get(ev.from);
  if (!entry) return;
  try { await entry.pc.setRemoteDescription({ type: 'answer', sdp: ev.sdp }); }
  catch (err) { console.warn('[gcall answer]', err); }
}

async function gcallOnIce(ev) {
  const entry = state.gcall?.peers.get(ev.from);
  if (!entry) return;
  try { await entry.pc.addIceCandidate(ev.candidate); }
  catch (err) { console.warn('[gcall ice]', err); }
}

function gcallOnLeave(ev) {
  if (!state.gcall || ev.call_id !== state.gcall.id) return;
  const generation = callLegGeneration(ev);
  const currentGeneration = state.gcall.memberGenerations.get(ev.from) || 0;
  // Once a generation-aware Join has been observed, a legacy/older Leave can
  // only be a delayed event from a superseded gateway incarnation.
  if (generation === 0 && currentGeneration > 0) return;
  if (generation > 0 && generation < currentGeneration) return;
  state.gcall.memberGenerations.delete(ev.from);
  removeGcallPeer(ev.from);
}

function callLegGeneration(ev) {
  const generation = Number(ev?.leg_generation);
  return Number.isSafeInteger(generation) && generation > 0 ? generation : 0;
}

function removeGcallPeer(peerId) {
  const entry = state.gcall?.peers.get(peerId);
  if (!entry) return;
  if (state.gcall?.mode !== 'sfu') {
    try { entry.pc.close(); } catch { /* peer may already be closed */ }
  }
  state.gcall.peers.delete(peerId);
  const tile = els.gcallGrid.querySelector(`[data-peer="${cssEscape(peerId)}"]`);
  if (tile) tile.remove();
  updateGcallCount();
}

function gcallToggleTrack(kind) {
  const s = state.gcall?.localStream;
  if (!s) return;
  for (const t of s.getTracks()) if (t.kind === kind) t.enabled = !t.enabled;
}

// ---------- screen share (group mesh) ----------
// Swaps the outgoing video on *every* peer's video sender to the screen track
// (replaceTrack — no new m-line for the common video-call case). Audio-only mesh
// calls fall back to addTrack, which fires negotiationneeded → per-peer re-offer.
// E2E correctness needs two real browsers (staging seam); locally we keep the
// per-peer sender state and signaling self-consistent.
async function gcallToggleScreenShare() {
  const g = state.gcall;
  if (!g) return;
  if (g.screenTrack) { gcallStopScreenShare(); return; }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia) {
    toast('当前浏览器不支持屏幕共享', 'error');
    return;
  }
  let display;
  try {
    display = await navigator.mediaDevices.getDisplayMedia({ video: true });
  } catch (err) {
    if (err && err.name !== 'NotAllowedError' && err.name !== 'AbortError') {
      toast(`屏幕共享失败:${err.message}`, 'error');
    }
    return;
  }
  const screenTrack = display.getVideoTracks()[0];
  if (!screenTrack) { toast('未获取到屏幕画面', 'error'); return; }
  g.screenTrack = screenTrack;
  g.screenStream = display;
  if (g.mode === 'sfu') {
    try {
      await ensureSfuGroup().useScreenTrack(screenTrack);
    } catch (err) {
      console.warn('[sfu share]', err);
      toast(`屏幕共享失败:${err.message}`, 'error');
    }
  } else {
    g.camTrack = null;
    for (const [, entry] of g.peers) {
      const sender = entry.pc.getSenders().find((s) => s.track && s.track.kind === 'video');
      if (sender) {
        if (!g.camTrack) g.camTrack = sender.track; // all peers share the same cam track
        sender.replaceTrack(screenTrack).catch((err) => console.warn('[gcall share]', err));
      } else {
        try { entry.pc.addTrack(screenTrack, g.localStream); } catch (err) { console.warn('[gcall share add]', err); }
      }
    }
  }
  // Show what we're sharing in our own tile; mark the button active.
  addGcallTile('me', '我', display, true);
  els.gcallShare?.classList.add('active');
  screenTrack.addEventListener('ended', () => { if (state.gcall?.screenTrack === screenTrack) gcallStopScreenShare(); });
}

function gcallStopScreenShare() {
  const g = state.gcall;
  if (!g || !g.screenTrack) return;
  const screenTrack = g.screenTrack;
  g.screenTrack = null;
  g.screenStream = null;
  try { screenTrack.stop(); } catch { /* track may already be ended */ }
  if (g.mode === 'sfu') {
    ensureSfuGroup()
      .restoreCameraTrack(g.camTrack || null)
      .catch((err) => console.warn('[sfu unshare]', err));
  } else {
    for (const [, entry] of g.peers) {
      const sender = entry.pc.getSenders().find((s) => s.track === screenTrack)
        || entry.pc.getSenders().find((s) => s.track && s.track.kind === 'video');
      if (sender) sender.replaceTrack(g.camTrack || null).catch((err) => console.warn('[gcall unshare]', err));
    }
    g.camTrack = null;
  }
  addGcallTile('me', '我', g.localStream, true);
  els.gcallShare?.classList.remove('active');
}

function leaveGroupCall() {
  const g = state.gcall;
  if (!g) { els.gcallOverlay.hidden = true; return; }
  if (g.id) { try { ws.callLeave(g.id, g.roomId); } catch { /* socket may be gone */ } }
  try { g.screenTrack?.stop(); } catch { /* best-effort cleanup */ }
  if (g.mode === 'sfu') {
    sfuGroup?.close();
  } else {
    for (const [, entry] of g.peers) { try { entry.pc.close(); } catch { /* best-effort cleanup */ } }
  }
  try { g.localStream?.getTracks().forEach((t) => t.stop()); } catch { /* best-effort cleanup */ }
  els.gcallGrid.replaceChildren();
  els.gcallShare?.classList.remove('active');
  els.gcallOverlay.hidden = true;
  state.gcall = null;
}

function addGcallTile(peerId, label, stream, muted) {
  let tile = els.gcallGrid.querySelector(`[data-peer="${cssEscape(peerId)}"]`);
  if (!tile) {
    tile = document.createElement('div');
    tile.className = 'call-tile';
    tile.dataset.peer = peerId;
    const v = document.createElement('video');
    v.autoplay = true; v.playsInline = true; v.muted = !!muted;
    const lab = document.createElement('span');
    lab.className = 'label';
    lab.textContent = label;
    tile.appendChild(v); tile.appendChild(lab);
    els.gcallGrid.appendChild(tile);
  }
  tile.querySelector('video').srcObject = stream;
  updateGcallCount();
}

function updateGcallCount() {
  if (!state.gcall) return;
  els.gcallCount.textContent = `${state.gcall.peers.size + 1} 人`;
}

function ensureSfuGroup() {
  if (sfuGroup) return sfuGroup;
  sfuGroup = new SfuGroupController({
    ws,
    getCall: () => state.gcall,
    getSelfId: () => state.me?.id,
    rtcConfig: rtcIceConfig,
    onSubscriptions: renderSfuSubscriptions,
    onConnectionState: (connectionState) => {
      if (connectionState === 'failed') toast('SFU 媒体连接失败，请重新加入通话', 'error');
    },
    onError: (error) => {
      console.warn('[sfu call]', error);
      toast(`SFU 协商失败:${error.message}`, 'error');
    },
  });
  return sfuGroup;
}

function renderSfuSubscriptions(subscriptions) {
  const g = state.gcall;
  if (!g || g.mode !== 'sfu') return;
  const byPublisher = new Map();
  for (const subscription of subscriptions) {
    const track = subscription.transceiver?.receiver?.track;
    if (!track || track.readyState === 'ended') continue;
    if (!byPublisher.has(subscription.publisher)) byPublisher.set(subscription.publisher, []);
    byPublisher.get(subscription.publisher).push(track);
  }
  for (const peerId of Array.from(g.peers.keys())) {
    if (byPublisher.has(peerId)) continue;
    g.peers.delete(peerId);
    els.gcallGrid
      .querySelector(`[data-peer="${cssEscape(peerId)}"]`)
      ?.remove();
  }
  for (const [peerId, tracks] of byPublisher) {
    const stream = new MediaStream(tracks);
    g.peers.set(peerId, { stream });
    const name = state.participants.get(peerId)?.display_name || peerId.slice(0, 6);
    addGcallTile(peerId, name, stream, false);
  }
  updateGcallCount();
}
