// media.js — file attachments, drag-and-drop upload, and voice recording.
//
// Extracted from app.js. These three flows all turn a local File/Blob into an
// optimistic message + a sendMessage frame, so they share the same app-core
// callbacks (optimisticAdd / clearReply / forceReauth), injected once via
// initMedia() to avoid a circular import back into app.js.
//
// Public surface:
//   • initMedia({ optimisticAdd, clearReply, forceReauth }) — wire all listeners

import { api, ApiError } from './api.js';
import { state, ws, els } from './context.js';
import { toast } from './render.js';

// app-core callbacks, injected by initMedia()
let optimisticAdd = () => {};
let clearReply = () => {};
let forceReauth = () => {};

const voiceState = { rec: null, chunks: [], started: 0, stream: null };
let btnVoice = null;

export function initMedia(deps) {
  if (deps) {
    if (typeof deps.optimisticAdd === 'function') optimisticAdd = deps.optimisticAdd;
    if (typeof deps.clearReply === 'function') clearReply = deps.clearReply;
    if (typeof deps.forceReauth === 'function') forceReauth = deps.forceReauth;
  }

  // ---------- attachments ----------
  els.btnAttach.addEventListener('click', () => els.fileInput.click());
  els.fileInput.addEventListener('change', async () => {
    const f = els.fileInput.files?.[0];
    if (!f) return;
    els.fileInput.value = '';
    await uploadAndSend(f);
  });

  // ---------- voice recording ----------
  btnVoice = document.getElementById('btn-voice');
  if (btnVoice) {
    btnVoice.addEventListener('click', toggleRecording);
  }

  // ---------- drag-and-drop file upload into the message area ----------
  ['dragenter', 'dragover'].forEach((evt) => {
    els.msgScroll.addEventListener(evt, (e) => {
      if (!e.dataTransfer || !Array.from(e.dataTransfer.types || []).includes('Files')) return;
      e.preventDefault();
      els.msgScroll.classList.add('drop-active');
    });
  });
  ['dragleave', 'dragend'].forEach((evt) => {
    els.msgScroll.addEventListener(evt, () => els.msgScroll.classList.remove('drop-active'));
  });
  els.msgScroll.addEventListener('drop', async (e) => {
    if (!e.dataTransfer) return;
    e.preventDefault();
    els.msgScroll.classList.remove('drop-active');
    const files = Array.from(e.dataTransfer.files || []);
    for (const f of files) {
      // serial to keep order
      // eslint-disable-next-line no-await-in-loop
      await uploadAndSend(f);
    }
  });
}

async function uploadAndSend(file) {
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  toast(`上传 ${file.name}…`, 'info');
  try {
    const blob = await api.uploadBlob(file);
    const block = {
      type: 'file',
      blob_id: blob.id,
      kind: blob.kind,
      name: blob.name,
      size: blob.size,
    };
    optimisticAdd(state.currentRoomId, [block]);
    ws.sendMessage(state.currentRoomId, [block], null);
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`上传失败:${err.message}`, 'error');
  }
}

async function toggleRecording() {
  if (voiceState.rec) {
    stopRecording();
    return;
  }
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  if (!navigator.mediaDevices || !window.MediaRecorder) {
    toast('浏览器不支持录音', 'error'); return;
  }
  try {
    voiceState.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const mime = MediaRecorder.isTypeSupported('audio/webm;codecs=opus') ? 'audio/webm;codecs=opus' : '';
    voiceState.rec = mime ? new MediaRecorder(voiceState.stream, { mimeType: mime }) : new MediaRecorder(voiceState.stream);
    voiceState.chunks = [];
    voiceState.started = Date.now();
    voiceState.rec.addEventListener('dataavailable', (e) => {
      if (e.data && e.data.size > 0) voiceState.chunks.push(e.data);
    });
    voiceState.rec.addEventListener('stop', onVoiceStop);
    voiceState.rec.start(250);
    btnVoice.classList.add('recording');
    btnVoice.textContent = '⏹';
    btnVoice.title = '点击停止';
    toast('录音中…', 'info');
  } catch (err) {
    toast(`录音失败:${err.message}`, 'error');
    cleanupVoice();
  }
}

function stopRecording() {
  if (voiceState.rec && voiceState.rec.state !== 'inactive') {
    voiceState.rec.stop();
  }
}

async function onVoiceStop() {
  const durationMs = Date.now() - voiceState.started;
  const blob = new Blob(voiceState.chunks, { type: voiceState.rec?.mimeType || 'audio/webm' });
  cleanupVoice();
  if (blob.size < 200) {
    toast('录音太短', 'error');
    return;
  }
  if (!state.currentRoomId) return;
  toast('上传录音…', 'info');
  try {
    const file = new File([blob], `voice-${Date.now()}.webm`, { type: blob.type });
    const meta = await api.uploadBlob(file);
    const voiceBlock = {
      type: 'voice',
      blob_id: meta.id,
      duration_ms: durationMs,
    };
    optimisticAdd(state.currentRoomId, [voiceBlock]);
    ws.sendMessage(state.currentRoomId, [voiceBlock], state.replyTo ? state.replyTo.id : null);
    clearReply();
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`上传失败:${err.message}`, 'error');
  }
}

function cleanupVoice() {
  if (voiceState.stream) {
    for (const t of voiceState.stream.getTracks()) t.stop();
  }
  voiceState.rec = null;
  voiceState.chunks = [];
  voiceState.stream = null;
  if (btnVoice) {
    btnVoice.classList.remove('recording');
    btnVoice.textContent = '🎙';
    btnVoice.title = '按住录音 / 点击开始';
  }
}
