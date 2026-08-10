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
import { snaplinkPasswordLogin, SnaplinkMfaRequired } from './snaplink_auth.js';

let enterChat = () => {};
let showAuth = () => {};
let onLogout = () => {};
let authConfig = { login_page: 'both', snaplink: null };
let loginPageMode = 'both';
let authConfigReady = Promise.resolve();

export function initAuthUi(deps) {
  if (deps) {
    if (typeof deps.enterChat === 'function') enterChat = deps.enterChat;
    if (typeof deps.showAuth === 'function') showAuth = deps.showAuth;
    if (typeof deps.onLogout === 'function') onLogout = deps.onLogout;
  }

  if (els.btnSso) {
    els.btnSso.addEventListener('click', (event) => {
      // The server owns the confidential browser flow and its HttpOnly PKCE
      // cookies. The SDK-backed flow is used by the Aero-owned page; this link
      // intentionally remains a navigation to the hosted Snaplink page for
      // `both`/`snaplink`.
      if (loginPageMode !== 'local') return;
      event.preventDefault();
    });
  }
  // A missing config document must not hide a working legacy login page.
  authConfigReady = api.authConfig().then((config) => {
    if (!config || typeof config !== 'object') return;
    authConfig = config;
    loginPageMode = ['local', 'snaplink', 'both'].includes(config.login_page)
      ? config.login_page
      : 'both';
    applyLoginPageMode();
  }).catch(() => {});

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
      // Wait for policy resolution so a fast first submit cannot accidentally
      // use Aero's legacy password endpoint in an SDK-backed deployment.
      await authConfigReady;
      const result = loginPageMode !== 'snaplink' && authConfig.snaplink
        ? await snaplinkPasswordLogin(authConfig, {
          username: email,
          password,
          secondFactor,
        })
        : await api.login({ email, password, ...factor });
      onAuthSuccess(result);
    } catch (err) {
      if (isTwoFactorError(err) || err instanceof SnaplinkMfaRequired) {
        setBusy(els.formLogin, false);
        els.loginSecondFactor.value = '';
        els.loginSecondFactor.focus();
        toast('请输入 6 位 TOTP 验证码或 Snaplink 恢复码', 'error');
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
    onLogout(); // drop in-memory draft state before the session is torn down
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

function applyLoginPageMode() {
  const local = loginPageMode === 'local';
  const snaplink = loginPageMode === 'snaplink';
  const sdkLocal = !snaplink && Boolean(authConfig.snaplink);
  if (els.authSsoOption) els.authSsoOption.hidden = local;
  if (els.authModeHint) {
    els.authModeHint.hidden = !sdkLocal && !snaplink;
    els.authModeHint.textContent = sdkLocal
      ? 'Aero 自有登录页面 · 账号由 Snaplink SDK 验证'
      : '正在使用 Snaplink 托管登录页面';
  }
  if (els.authTabs) els.authTabs.hidden = snaplink;
  if (els.formLogin) els.formLogin.hidden = snaplink;
  if (els.formRegister) els.formRegister.hidden = snaplink || sdkLocal;
  if (els.tabs) {
    for (const tab of els.tabs) {
      const isRegister = tab.dataset.tab === 'register';
      tab.hidden = sdkLocal && isRegister;
      tab.disabled = tab.hidden;
    }
  }
  if (sdkLocal && els.formLogin) {
    for (const tab of els.tabs || []) tab.classList.toggle('active', tab.dataset.tab === 'login');
  }
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
