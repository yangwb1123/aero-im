// chrome.js — global modal/drawer dismissal wiring (extracted from app.js).
//
// Backdrop-click + [data-close] + Escape close every modal; [data-close-drawer]
// buttons + Escape close every drawer. Self-contained: only touches els +
// closeModal + the emoji picker's closer.
//
// Public surface:
//   • initChrome() — attach all close listeners (call once)

import { els, closeModal } from './context.js';
import { closeEmojiPicker } from './emoji.js';

export function initChrome() {
  // ---------- modal close ----------
  for (const m of [els.modalNewRoom, els.modalAddMember, els.modalGoLive, els.modalStreamInfo, els.modalProfile]) {
    if (!m) continue;
    m.addEventListener('click', (e) => {
      if (e.target === m) closeModal(m);
      if (e.target instanceof HTMLElement && e.target.hasAttribute('data-close')) closeModal(m);
    });
  }
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      for (const m of [els.modalNewRoom, els.modalAddMember, els.modalGoLive, els.modalStreamInfo, els.modalProfile])
        if (m && !m.hidden) closeModal(m);
      for (const d of [els.drawerSearch, els.drawerAi, els.drawerLive, els.drawerNotif])
        if (d && !d.hidden) d.hidden = true;
      closeEmojiPicker();
    }
  });

  // ---------- drawer close ----------
  document.querySelectorAll('[data-close-drawer]').forEach((b) => {
    b.addEventListener('click', () => {
      const k = b.dataset.closeDrawer;
      if (k === 'search') els.drawerSearch.hidden = true;
      if (k === 'ai') els.drawerAi.hidden = true;
      if (k === 'live') els.drawerLive.hidden = true;
      if (k === 'notif') els.drawerNotif.hidden = true;
    });
  });
}
