// search.js — search drawer + AI assistant drawer (extracted from app.js).
//
// Depends only on the shared spine (state / els / context leaf utils +
// scrollToMessage), the api client, and render's `toast`. No back-references
// into app-core, so no circular imports.
//
// Public surface:
//   • initSearchAi() — wire the search/AI button + input listeners (call once)
//   • restoreAiHistory(roomId) — repaint the per-room AI transcript on room switch

import { api } from './api.js';
import { state, els, loadingDiv, mutedDiv, formatTime, scrollToMessage } from './context.js';
import { toast } from './render.js';

// Attach the search + AI drawer event listeners. Called once at bootstrap so
// the wiring lives with the logic (was top-level in app.js originally).
export function initSearchAi() {
  // ---------- search ----------
  els.btnSearch.addEventListener('click', () => {
    if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
    els.drawerSearch.hidden = false;
    els.searchInput.focus();
  });
  els.searchInput.addEventListener('keydown', async (e) => {
    if (e.key !== 'Enter') return;
    const q = els.searchInput.value.trim();
    if (!q) return;
    els.searchResults.replaceChildren(loadingDiv('搜索中…'));
    try {
      const res = await api.search(state.currentRoomId, { query: q, limit: 30, mode: 'auto' });
      const hits = res?.results || [];
      els.searchResults.replaceChildren();
      if (!hits.length) { els.searchResults.appendChild(mutedDiv('无结果')); return; }
      for (const h of hits) {
        const m = h.message;
        const wrap = document.createElement('div'); wrap.className = 'search-hit';
        const meta = document.createElement('div'); meta.className = 'search-hit-meta';
        meta.textContent = `${state.participants.get(m.sender_id)?.display_name || m.sender_id?.slice(0,6)} · ${formatTime(m.created_at)} · score ${h.score.toFixed(2)}`;
        const text = document.createElement('div'); text.className = 'search-hit-text';
        text.textContent = (m.blocks || []).map((b) => b.content || '').join(' ').slice(0, 240);
        wrap.appendChild(meta); wrap.appendChild(text);
        wrap.addEventListener('click', () => {
          els.drawerSearch.hidden = true;
          scrollToMessage(m.id);
        });
        els.searchResults.appendChild(wrap);
      }
    } catch (err) {
      els.searchResults.replaceChildren(mutedDiv(`搜索失败:${err.message}`));
    }
  });

  // ---------- AI ----------
  els.btnAi.addEventListener('click', () => {
    if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
    els.drawerAi.hidden = false;
    els.aiInput.focus();
  });
  els.btnAiSummarize.addEventListener('click', async () => {
    if (!state.currentRoomId) return;
    appendAiMessage('生成摘要中…', '');
    try {
      const res = await api.aiSummarize(state.currentRoomId, 80);
      appendAiMessage('房间摘要', res.summary);
    } catch (err) {
      appendAiMessage('摘要失败', err.message);
    }
  });
  els.aiInput.addEventListener('keydown', async (e) => {
    if (e.key !== 'Enter') return;
    const q = els.aiInput.value.trim();
    if (!q) return;
    els.aiInput.value = '';
    appendAiMessage(q, '思考中…');
    try {
      const res = await api.aiAsk(state.currentRoomId, q, 8);
      appendAiMessage(q, res.answer, res.citations || []);
    } catch (err) {
      appendAiMessage(q, `失败:${err.message}`);
    }
  });
}

function appendAiMessage(q, a, citations = []) {
  const wrap = document.createElement('div'); wrap.className = 'ai-message';
  const qEl = document.createElement('div'); qEl.className = 'ai-q'; qEl.textContent = q;
  const aEl = document.createElement('div'); aEl.className = 'ai-a'; aEl.textContent = a;
  wrap.appendChild(qEl); wrap.appendChild(aEl);
  if (citations.length) {
    const row = document.createElement('div');
    for (const c of citations) {
      const chip = document.createElement('span'); chip.className = 'ai-cite';
      chip.textContent = '↗ ' + String(c).slice(0, 6);
      chip.addEventListener('click', () => { els.drawerAi.hidden = true; scrollToMessage(c); });
      row.appendChild(chip);
    }
    wrap.appendChild(row);
  }
  els.aiResults.appendChild(wrap);
  els.aiResults.scrollTop = els.aiResults.scrollHeight;
  saveAiHistory(state.currentRoomId, { q, a, citations });
}

function aiStoreKey(roomId) { return 'aero_ai_history:' + roomId; }

function saveAiHistory(roomId, entry) {
  if (!roomId) return;
  try {
    const k = aiStoreKey(roomId);
    const raw = sessionStorage.getItem(k);
    const arr = raw ? JSON.parse(raw) : [];
    arr.push({ ...entry, ts: Date.now() });
    if (arr.length > 50) arr.shift();
    sessionStorage.setItem(k, JSON.stringify(arr));
  } catch (e) { /* quota; ignore */ }
}

export function restoreAiHistory(roomId) {
  els.aiResults.replaceChildren();
  if (!roomId) return;
  try {
    const k = aiStoreKey(roomId);
    const raw = sessionStorage.getItem(k);
    if (!raw) return;
    const arr = JSON.parse(raw);
    for (const e of arr) {
      const wrap = document.createElement('div'); wrap.className = 'ai-message';
      const qEl = document.createElement('div'); qEl.className = 'ai-q'; qEl.textContent = e.q;
      const aEl = document.createElement('div'); aEl.className = 'ai-a'; aEl.textContent = e.a;
      wrap.appendChild(qEl); wrap.appendChild(aEl);
      if (Array.isArray(e.citations) && e.citations.length) {
        const row = document.createElement('div');
        for (const c of e.citations) {
          const chip = document.createElement('span'); chip.className = 'ai-cite';
          chip.textContent = '↗ ' + String(c).slice(0, 6);
          chip.addEventListener('click', () => { els.drawerAi.hidden = true; scrollToMessage(c); });
          row.appendChild(chip);
        }
        wrap.appendChild(row);
      }
      els.aiResults.appendChild(wrap);
    }
  } catch (err) { /* ignore */ }
}
