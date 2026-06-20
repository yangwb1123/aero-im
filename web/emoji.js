// emoji.js — lightweight emoji picker popover (extracted from app.js).
//
// Fully self-contained: no shared state, no api, no els. Caller passes the
// anchor element and an onPick(emoji) callback. Public surface:
//   • openEmojiPicker(anchor, onPick)
//   • closeEmojiPicker()

const EMOJIS = ['👍','❤️','😂','🎉','🚀','🔥','👀','🤔','✅','❌','💯','🙏','👏','😎','😢','😡','💪','🧠','🤖','✨'];
let emojiPop = null;

export function openEmojiPicker(anchor, onPick) {
  closeEmojiPicker();
  const pop = document.createElement('div'); pop.className = 'emoji-pop';
  for (const e of EMOJIS) {
    const b = document.createElement('button'); b.textContent = e;
    b.addEventListener('click', () => { onPick(e); closeEmojiPicker(); });
    pop.appendChild(b);
  }
  document.body.appendChild(pop);
  const r = anchor.getBoundingClientRect();
  pop.style.top = `${r.bottom + 6}px`;
  pop.style.left = `${Math.min(window.innerWidth - 300, r.left)}px`;
  emojiPop = pop;
  setTimeout(() => document.addEventListener('click', onDocClick, { once: true }), 0);
}

function onDocClick(e) {
  if (emojiPop && !emojiPop.contains(e.target)) closeEmojiPicker();
}

export function closeEmojiPicker() {
  if (emojiPop?.parentNode) emojiPop.parentNode.removeChild(emojiPop);
  emojiPop = null;
}
