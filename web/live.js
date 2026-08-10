// live.js — live-streams drawer (browse) + go-live flow (extracted from app.js).
//
// Depends on the shared spine (state / els / context helpers), the api client,
// and render's `toast`. No back-references into app-core.
//
// Public surface:
//   • initLive() — wire the live-page / go-live button + form listeners

import { api } from './api.js';
import { state, els, loadingDiv, mutedDiv, openModal, closeModal, setBusy } from './context.js';
import { attachHls, toast } from './render.js';

export function initLive() {
  els.btnLivePage.addEventListener('click', async () => {
    els.drawerLive.hidden = false;
    els.liveList.replaceChildren(loadingDiv('加载中…'));
    try {
      const list = await api.listStreams();
      els.liveList.replaceChildren();
      if (!list?.length) { els.liveList.appendChild(mutedDiv('当前无直播。')); return; }
      for (const s of list) {
        const card = document.createElement('div'); card.className = 'live-card';
        const video = document.createElement('video');
        video.controls = true; video.playsInline = true; video.muted = true;
        const src = s.hls_path || `/hls/${encodeURIComponent(s.id)}/index.m3u8`;
        if (!attachHls(video, src)) {
          // hls.js is unavailable or unsupported; retain the direct src fallback
          // on the hidden video for unusual native implementations.
          video.style.display = 'none';
          const ph = document.createElement('div'); ph.className = 'live-thumb';
          ph.style.cssText = 'display:grid;place-items:center;color:var(--text-mute);';
          ph.textContent = '当前浏览器不支持 HLS 播放';
          card.appendChild(ph);
        }
        card.appendChild(video);
        const body = document.createElement('div'); body.className = 'live-card-body';
        const title = document.createElement('div'); title.className = 'live-title';
        title.textContent = s.title || '(no title)';
        const status = document.createElement('span'); status.className = 'live-status'; status.textContent = 'LIVE';
        title.appendChild(status);
        const sub = document.createElement('div'); sub.className = 'live-sub';
        sub.textContent = `${s.protocol} · ${s.id.slice(0, 6)}`;
        body.appendChild(title); body.appendChild(sub);
        card.appendChild(body);
        els.liveList.appendChild(card);
      }
    } catch (err) {
      els.liveList.replaceChildren(mutedDiv(`加载失败:${err.message}`));
    }
  });

  els.btnGoLive.addEventListener('click', () => openModal(els.modalGoLive));
  els.formGoLive.addEventListener('submit', async (e) => {
    e.preventDefault();
    const fd = new FormData(els.formGoLive);
    const title = String(fd.get('title') || '').trim();
    const protocol = String(fd.get('protocol') || 'rtmp');
    setBusy(els.formGoLive, true);
    try {
      const res = await api.createStream({ title, protocol, room_id: state.currentRoomId });
      closeModal(els.modalGoLive);
      els.streamIngest.value = res.ingest_url;
      els.streamHls.value = res.hls_url;
      openModal(els.modalStreamInfo);
      els.formGoLive.reset();
    } catch (err) {
      toast(`创建失败:${err.message}`, 'error');
    } finally { setBusy(els.formGoLive, false); }
  });
}
