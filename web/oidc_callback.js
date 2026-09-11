// Completes the same-origin OIDC callback without putting Aero tokens in the URL.
(() => {
  'use strict';

  const payloadNode = document.getElementById('oidc-session');
  try {
    if (!payloadNode) throw new Error('missing OIDC session payload');
    const session = JSON.parse(payloadNode.textContent || '');
    if (
      typeof session.access_token !== 'string'
      || typeof session.refresh_token !== 'string'
      || typeof session.participant_id !== 'string'
      || !session.access_token
      || !session.refresh_token
      || !session.participant_id
    ) {
      throw new Error('invalid OIDC session payload');
    }

    localStorage.setItem('aero_token', session.access_token);
    localStorage.setItem('aero_refresh', session.refresh_token);
    localStorage.setItem('aero_pid', session.participant_id);
    payloadNode.remove();
    window.location.replace('/');
  } catch {
    payloadNode?.remove();
    try {
      localStorage.removeItem('aero_token');
      localStorage.removeItem('aero_refresh');
      localStorage.removeItem('aero_pid');
    } catch {
      // Storage may itself be unavailable; the callback still fails closed.
    }
    document.body.textContent = 'Snaplink SSO 登录完成失败，请返回后重试。';
  }
})();
