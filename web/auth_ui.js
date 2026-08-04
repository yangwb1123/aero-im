// auth_ui.js — login / register / logout form wiring (extracted from app.js).
//
// Owns the auth-view form submit handlers + the logout button. `enterChat` and
// `showAuth` are app-core view transitions (they touch many app-core routines),
// so they are injected once via initAuthUi() rather than imported, avoiding a
// circular dependency back into app.js.
//
// Public surface:
//   • initAuthUi({ enterChat, showAuth }) — wire login/register/logout

import { api, auth, ApiError } from './api.js';
import { state, ws, els, setBusy } from './context.js';
import { discardPersistedDeliveries } from './delivery.js';
import { toast } from './render.js';

let enterChat = () => {};
let showAuth = () => {};

export function initAuthUi(deps) {
  if (deps) {
    if (typeof deps.enterChat === 'function') enterChat = deps.enterChat;
    if (typeof deps.showAuth === 'function') showAuth = deps.showAuth;
  }

  els.formLogin.addEventListener('submit', async (e) => {
    e.preventDefault();
    const fd = new FormData(els.formLogin);
    const email = String(fd.get('email') || '').trim();
    const password = String(fd.get('password') || '');
    const secondFactor = String(fd.get('second_factor') || '').trim();
    const factor = /^\d{6}$/.test(secondFactor)
      ? { totp: secondFactor }
      : { recovery_code: secondFactor };
    if (!email || !password) return;
    setBusy(els.formLogin, true);
    try {
      onAuthSuccess(await api.login({ email, password, ...factor }));
    } catch (err) {
      if (isTwoFactorError(err)) {
        setBusy(els.formLogin, false);
        els.loginSecondFactor.value = '';
        els.loginSecondFactor.focus();
        toast('请输入有效的 6 位验证码或一次性恢复码', 'error');
      } else {
        toast(err.message || '登录失败', 'error');
      }
    }
    finally { setBusy(els.formLogin, false); }
  });

  els.formRegister.addEventListener('submit', async (e) => {
    e.preventDefault();
    const fd = new FormData(els.formRegister);
    const email = String(fd.get('email') || '').trim();
    const password = String(fd.get('password') || '');
    const display_name = String(fd.get('display_name') || '').trim();
    if (!email || !password || !display_name) return;
    setBusy(els.formRegister, true);
    try { onAuthSuccess(await api.register({ email, password, display_name })); }
    catch (err) { toast(err.message || '注册失败', 'error'); }
    finally { setBusy(els.formRegister, false); }
  });

  els.btnLogout.addEventListener('click', async () => {
    const refreshToken = auth.getRefresh();
    // Start revocation before clearing localStorage. The refresh token itself
    // authenticates logout, so this remains valid even if the access JWT expired.
    const revoke = refreshToken ? api.logout(refreshToken) : Promise.resolve();
    discardPersistedDeliveries(state.me?.id);
    ws.close();
    auth.clear();
    Object.assign(state, {
      me: null, currentRoomId: null,
      rooms: new Map(), participants: new Map(), messagesByRoom: new Map(),
      reachedTop: new Set(), pendingByTempId: new Map(),
      typing: new Map(), reactionsByMsg: new Map(), receiptsByRoom: new Map(),
      unreadByRoom: new Map(),
    });
    els.roomList.replaceChildren();
    els.msgList.replaceChildren();
    els.onlineList.replaceChildren();
    showAuth();
    try {
      await revoke;
    } catch (err) {
      toast(`本机已退出；服务端会话撤销失败:${err.message}`, 'error');
    }
  });
}

function onAuthSuccess(res) {
  if (!res?.access_token || !res?.participant) { toast('服务端返回不完整', 'error'); return; }
  auth.setSession(res.access_token, res.refresh_token, res.participant.id);
  els.formLogin.reset();
  state.me = res.participant;
  state.participants.set(res.participant.id, res.participant);
  enterChat();
}

function isTwoFactorError(err) {
  if (!(err instanceof ApiError) || err.status !== 401) return false;
  const detail = err.body?.msg || err.body?.message || err.message || '';
  return String(detail).includes('2fa_required');
}
