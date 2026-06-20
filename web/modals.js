// modals.js — profile / new-room / add-member form flows (extracted from app.js).
//
// Three small modal-backed forms that POST to the API and then reconcile shared
// state + the room list. They reach into a few app-core routines
// (forceReauth / refreshRoomList / switchRoom), injected once via
// initModalForms() to avoid a circular import back into app.js.
//
// Public surface:
//   • initModalForms({ forceReauth, refreshRoomList, switchRoom }) — wire all

import { api, ApiError } from './api.js';
import { state, els, openModal, closeModal, setBusy } from './context.js';
import { initialOf, toast } from './render.js';

let forceReauth = () => {};
let refreshRoomList = () => {};
let switchRoom = () => {};

export function initModalForms(deps) {
  if (deps) {
    if (typeof deps.forceReauth === 'function') forceReauth = deps.forceReauth;
    if (typeof deps.refreshRoomList === 'function') refreshRoomList = deps.refreshRoomList;
    if (typeof deps.switchRoom === 'function') switchRoom = deps.switchRoom;
  }

  // ---------- profile editing ----------
  els.meAvatar.addEventListener('click', () => {
    if (!state.me) return;
    els.formProfile.querySelector('[name="display_name"]').value = state.me.display_name || '';
    els.formProfile.querySelector('[name="avatar_url"]').value = state.me.avatar_url || '';
    openModal(els.modalProfile);
  });
  els.formProfile.addEventListener('submit', async (e) => {
    e.preventDefault();
    const fd = new FormData(els.formProfile);
    const display_name = String(fd.get('display_name') || '').trim();
    const avatar_url_raw = String(fd.get('avatar_url') || '').trim();
    if (!display_name) { toast('显示名不能空', 'error'); return; }
    setBusy(els.formProfile, true);
    try {
      const me = await api.updateMe({ display_name, avatar_url: avatar_url_raw || null });
      state.me = me;
      state.participants.set(me.id, me);
      els.meName.textContent = me.display_name || '—';
      els.meEmail.textContent = me.email || me.id || '';
      els.meAvatar.textContent = initialOf(me.display_name || me.email);
      closeModal(els.modalProfile);
      toast('已更新', 'ok');
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) forceReauth();
      else toast(`更新失败:${err.message}`, 'error');
    } finally { setBusy(els.formProfile, false); }
  });

  // ---------- new room ----------
  els.btnNewRoom.addEventListener('click', () => openModal(els.modalNewRoom));
  els.formNewRoom.addEventListener('submit', async (e) => {
    e.preventDefault();
    const fd = new FormData(els.formNewRoom);
    const kind = String(fd.get('kind') || 'group');
    const name = String(fd.get('name') || '').trim();
    setBusy(els.formNewRoom, true);
    try {
      const room = await api.createRoom({ kind, name });
      state.rooms.set(room.id, room);
      refreshRoomList();
      closeModal(els.modalNewRoom);
      els.formNewRoom.reset();
      toast(`已创建 ${room.kind} · ${room.id.slice(0, 8)}…`, 'ok');
      switchRoom(room.id);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) forceReauth();
      else toast(`创建失败:${err.message}`, 'error');
    } finally { setBusy(els.formNewRoom, false); }
  });

  // ---------- add member ----------
  els.btnAddMember.addEventListener('click', () => openModal(els.modalAddMember));
  els.formAddMember.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (!state.currentRoomId) return;
    const fd = new FormData(els.formAddMember);
    const pid = String(fd.get('participant_id') || '').trim();
    if (!pid) return;
    setBusy(els.formAddMember, true);
    try {
      await api.addMember(state.currentRoomId, pid);
      closeModal(els.modalAddMember);
      els.formAddMember.reset();
      toast('已添加成员', 'ok');
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) forceReauth();
      else toast(`添加失败:${err.message}`, 'error');
    } finally { setBusy(els.formAddMember, false); }
  });
}
