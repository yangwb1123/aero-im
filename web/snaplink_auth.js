// Snaplink SDK-backed login for the configurable Aero-owned login page.

import { api } from './api.js';
import { SSOClient, SSOError } from './vendor/snaplink_sso_client.js';

export class SnaplinkMfaRequired extends Error {
  constructor(challenge) {
    super('Snaplink 需要完成二次验证');
    this.name = 'SnaplinkMfaRequired';
    this.challenge = challenge;
  }
}

function requireIdToken(response) {
  if (!response || typeof response.id_token !== 'string' || !response.id_token) {
    throw new SSOError(502, 'invalid_response', 'Snaplink 未返回 OpenID ID token');
  }
  return response.id_token;
}

async function exchangeMfa(client, challenge, secondFactor) {
  if (!secondFactor) throw new SnaplinkMfaRequired(challenge);
  const method = /^\d{6}$/.test(secondFactor) ? 'totp' : 'recovery';
  return client.postMFAComplete({
    mfa_challenge_id: challenge.mfa_challenge_id,
    mfa_method: method,
    code: secondFactor,
  }).then(requireIdToken);
}

/**
 * Authenticate against Snaplink through its generated SDK operation surface,
 * then exchange the resulting ID token at Aero's own session boundary.
 *
 * The caller must provide the public auth-config document returned by Aero's
 * `/api/auth/config`; no Snaplink client secret ever reaches this module.
 */
export async function snaplinkPasswordLogin(config, { username, password, secondFactor = '' }) {
  if (!config?.snaplink) throw new SSOError(503, 'not_configured', 'Snaplink 登录未配置');
  const snaplink = config.snaplink;
  const client = new SSOClient({ baseUrl: snaplink.base_url, clientId: snaplink.client_id });
  // This deliberately uses the generated SDK's documented direct-login
  // operation. Snaplink returns an ID token alongside the access token when
  // the `openid` scope is requested; no client secret or authorization code is
  // exposed to the browser.
  const response = await client.login(username, password, { scope: snaplink.scope });
  const idToken = response?.error === 'mfa_required'
    ? await exchangeMfa(client, response, secondFactor)
    : requireIdToken(response);

  return api.oidcLogin(idToken);
}
