import test from 'node:test';
import assert from 'node:assert/strict';

import { api } from './api.js';
import {
  formatSecurityTime,
  isWorkspaceAdmin,
  memberSessionRevokeControl,
  normalizeAutoModRules,
  normalizeScimTokenList,
  normalizeWorkspaceList,
  scimCredentialBelongsToWorkspace,
  securityAccessForRole,
  sessionIdFromJwt,
  workspaceGenerationMatches,
} from './security_admin_utils.js';

test('workspace security role matrix is fail-closed', () => {
  assert.equal(isWorkspaceAdmin('owner'), true);
  assert.equal(isWorkspaceAdmin('Admin'), true);
  assert.equal(isWorkspaceAdmin('member'), false);
  assert.equal(isWorkspaceAdmin('guest'), false);
  assert.equal(isWorkspaceAdmin(undefined), false);

  assert.deepEqual(securityAccessForRole('owner'), {
    readPolicy: true,
    manageWorkspace: true,
  });
  assert.deepEqual(securityAccessForRole('member'), {
    readPolicy: true,
    manageWorkspace: false,
  });
  assert.deepEqual(securityAccessForRole('unexpected'), {
    readPolicy: false,
    manageWorkspace: false,
  });
});

test('global member-session revoke controls are owner-only and fail closed', () => {
  for (const target of ['owner', 'admin', 'member', 'guest']) {
    assert.deepEqual(memberSessionRevokeControl('owner', target), {
      visible: true,
      disabled: false,
      reason: '',
    });
  }

  const adminControl = memberSessionRevokeControl('admin', 'member');
  assert.equal(adminControl.visible, true);
  assert.equal(adminControl.disabled, true);
  assert.match(adminControl.reason, /Owner/);
  assert.equal(memberSessionRevokeControl('admin', 'owner').disabled, true);

  assert.deepEqual(memberSessionRevokeControl('member', 'member'), {
    visible: false,
    disabled: true,
    reason: '',
  });
  assert.equal(memberSessionRevokeControl('owner', 'unexpected').disabled, true);
  assert.equal(memberSessionRevokeControl(undefined, 'member').visible, false);
});

test('refresh JWT session id parsing accepts base64url and rejects malformed input', () => {
  const encoded = globalThis.btoa(JSON.stringify({ sub: 'participant', sid: 'session-123' }))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/g, '');
  assert.equal(sessionIdFromJwt(`header.${encoded}.signature`), 'session-123');
  assert.equal(sessionIdFromJwt('not-a-jwt'), null);
  assert.equal(sessionIdFromJwt('header.invalid!.signature'), null);

  const noSession = globalThis.btoa(JSON.stringify({ sub: 'participant' }));
  assert.equal(sessionIdFromJwt(`header.${noSession}.signature`), null);
});

test('workspace normalization and time formatting tolerate bad API values', () => {
  const valid = { id: 'workspace-1', name: 'Aero' };
  assert.deepEqual(normalizeWorkspaceList([null, {}, { id: '' }, valid]), [valid]);
  assert.deepEqual(normalizeWorkspaceList({ items: [valid] }), []);
  assert.equal(formatSecurityTime('not-a-date'), '—');
  assert.notEqual(formatSecurityTime('2026-07-28T12:00:00Z'), '—');
});

test('enterprise credential and moderation lists whitelist safe fields', () => {
  assert.deepEqual(normalizeScimTokenList({
    tokens: [{
      id: 'token-1',
      workspace_id: 'workspace-1',
      label: 'Okta',
      created_at: '2026-07-28T12:00:00Z',
      revoked_at: '2026-07-29T12:00:00Z',
      token: 'must-not-reach-ui-state',
      token_hash: 'must-not-reach-ui-state',
    }],
  }), [{
    id: 'token-1',
    workspace_id: 'workspace-1',
    label: 'Okta',
    created_at: '2026-07-28T12:00:00Z',
    revoked_at: '2026-07-29T12:00:00Z',
  }]);
  assert.deepEqual(normalizeScimTokenList({ tokens: [{ label: 'missing id' }] }), []);

  assert.deepEqual(normalizeAutoModRules({
    rules: [{
      id: 'rule-1',
      workspace_id: 'workspace-1',
      pattern: 'blocked',
      match_type: 'prefix',
      action: 'block',
      created_by: 'not-needed-in-ui',
    }],
  }), [{
    id: 'rule-1',
    pattern: 'blocked',
    match_type: 'prefix',
    action: 'block',
  }]);
  assert.deepEqual(normalizeAutoModRules({ rules: [{ id: 'rule-2', action: 'warn' }] }), [{
    id: 'rule-2',
    pattern: '',
    match_type: 'unknown',
    action: 'unknown',
  }]);
});

test('workspace generations and one-time SCIM secrets fail closed across switches', () => {
  const context = { workspaceId: 'workspace-a', loadId: 7 };
  assert.equal(workspaceGenerationMatches(context, 'workspace-a', 7), true);
  assert.equal(workspaceGenerationMatches(context, 'workspace-b', 7), false);
  assert.equal(workspaceGenerationMatches(context, 'workspace-a', 8), false);
  assert.equal(workspaceGenerationMatches(null, 'workspace-a', 7), false);

  const credential = {
    id: 'token-1',
    workspace_id: 'workspace-a',
    token: 'scim_secret',
  };
  assert.equal(scimCredentialBelongsToWorkspace(credential, 'workspace-a'), true);
  assert.equal(scimCredentialBelongsToWorkspace(credential, 'workspace-b'), false);
  assert.equal(
    scimCredentialBelongsToWorkspace({ ...credential, token: '' }, 'workspace-a'),
    false,
  );
  assert.equal(scimCredentialBelongsToWorkspace(null, 'workspace-a'), false);
});

test('enterprise security API wrappers preserve method, scope, encoding, and bodies', async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const requests = [];
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
      getItem: (key) => key === 'aero_token' ? 'access.jwt' : null,
    },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init });
    const noContent = init.method === 'DELETE';
    const saml = url === '/saml/metadata';
    return {
      status: noContent ? 204 : 200,
      ok: true,
      headers: {
        get: () => saml ? 'application/samlmetadata+xml' : 'application/json',
      },
      json: async () => ({ ok: true }),
      text: async () => '<EntityDescriptor />',
    };
  };

  try {
    await api.listWorkspaces();
    await api.listWorkspaceMembers('ws /1');
    await api.workspaceSecurity('ws /1');
    await api.setWorkspaceSecurity('ws /1', true);
    await api.workspaceStorageRegion('ws /1');
    await api.setWorkspaceStorageRegion('ws /1', 'eu-west-1');
    await api.ipAllowlist('ws /1');
    await api.addIpAllowlist('ws /1', { cidr: '10.0.0.0/8', note: 'office' });
    await api.removeIpAllowlist('ws /1', '10.0.0.0/8');
    await api.mintScimToken('ws /1', 'Okta');
    await api.listScimTokens('ws /1');
    await api.revokeScimToken('token /1');
    await api.listAutoModRules('ws /1');
    await api.createAutoModRule('ws /1', {
      pattern: 'blocked',
      match_type: 'prefix',
      action: 'block',
    });
    await api.deleteAutoModRule('ws /1', 'rule /1');
    await api.listSessions();
    await api.revokeSession('session /1');
    await api.revokeOtherSessions('refresh.jwt');
    await api.revokeMemberSessions('ws /1', 'participant /1');
    await api.samlMetadata();
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map(({ init, url }) => [init.method, url]), [
    ['GET', '/api/workspaces'],
    ['GET', '/api/workspaces/ws%20%2F1/members'],
    ['GET', '/api/workspaces/ws%20%2F1/security'],
    ['PUT', '/api/workspaces/ws%20%2F1/security'],
    ['GET', '/api/workspaces/ws%20%2F1/storage-region'],
    ['PUT', '/api/workspaces/ws%20%2F1/storage-region'],
    ['GET', '/api/workspaces/ws%20%2F1/ip-allowlist'],
    ['POST', '/api/workspaces/ws%20%2F1/ip-allowlist'],
    ['DELETE', '/api/workspaces/ws%20%2F1/ip-allowlist'],
    ['POST', '/api/workspaces/ws%20%2F1/scim/token'],
    ['GET', '/api/workspaces/ws%20%2F1/scim/tokens'],
    ['DELETE', '/api/scim/tokens/token%20%2F1'],
    ['GET', '/api/workspaces/ws%20%2F1/auto-mod-rules'],
    ['POST', '/api/workspaces/ws%20%2F1/auto-mod-rules'],
    ['DELETE', '/api/workspaces/ws%20%2F1/auto-mod-rules/rule%20%2F1'],
    ['GET', '/api/auth/sessions'],
    ['DELETE', '/api/auth/sessions/session%20%2F1'],
    ['POST', '/api/auth/sessions/revoke-others'],
    ['POST', '/api/workspaces/ws%20%2F1/members/participant%20%2F1/revoke-sessions'],
    ['GET', '/saml/metadata'],
  ]);
  assert.deepEqual(JSON.parse(requests[3].init.body), { require_2fa: true });
  assert.deepEqual(JSON.parse(requests[5].init.body), { storage_region: 'eu-west-1' });
  assert.deepEqual(JSON.parse(requests[7].init.body), {
    cidr: '10.0.0.0/8',
    note: 'office',
  });
  assert.deepEqual(JSON.parse(requests[8].init.body), { cidr: '10.0.0.0/8' });
  assert.deepEqual(JSON.parse(requests[9].init.body), { label: 'Okta' });
  assert.deepEqual(JSON.parse(requests[13].init.body), {
    pattern: 'blocked',
    match_type: 'prefix',
    action: 'block',
  });
  assert.deepEqual(JSON.parse(requests[17].init.body), {
    current_refresh_token: 'refresh.jwt',
  });
  assert.equal(requests[0].init.headers.Authorization, 'Bearer access.jwt');
  assert.equal(requests[19].init.headers.Authorization, undefined);
});
