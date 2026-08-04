// Shared helpers for the enterprise security UI. Pure helpers stay importable
// under Node; browser-only DOM access happens only when securityNodes() is called.

import { ApiError, auth } from './api.js';
import { toast } from './render.js';

export function isWorkspaceAdmin(role) {
  const normalized = String(role || '').toLowerCase();
  return normalized === 'owner' || normalized === 'admin';
}

export function securityAccessForRole(role) {
  const normalized = String(role || '').toLowerCase();
  return {
    readPolicy: ['owner', 'admin', 'member', 'guest'].includes(normalized),
    manageWorkspace: isWorkspaceAdmin(normalized),
  };
}

export function memberSessionRevokeControl(callerRole, targetRole) {
  const caller = String(callerRole || '').toLowerCase();
  const target = String(targetRole || '').toLowerCase();
  const knownRoles = ['owner', 'admin', 'member', 'guest'];
  if (!['owner', 'admin'].includes(caller)) {
    return { visible: false, disabled: true, reason: '' };
  }
  if (caller !== 'owner') {
    return {
      visible: true,
      disabled: true,
      reason: '该操作会影响成员在所有工作区的登录，仅 Owner 可执行。',
    };
  }
  if (!knownRoles.includes(target)) {
    return {
      visible: true,
      disabled: true,
      reason: '成员角色无效，已阻止全局会话撤销。',
    };
  }
  return { visible: true, disabled: false, reason: '' };
}

export function normalizeWorkspaceList(value) {
  if (!Array.isArray(value)) return [];
  return value.filter((workspace) => (
    workspace
    && typeof workspace === 'object'
    && typeof workspace.id === 'string'
    && workspace.id.trim()
  ));
}

export function normalizeScimTokenList(value) {
  const tokens = Array.isArray(value?.tokens) ? value.tokens : [];
  return tokens
    .filter((token) => token && typeof token === 'object' && String(token.id || '').trim())
    .map((token) => ({
      id: String(token.id),
      workspace_id: typeof token.workspace_id === 'string' ? token.workspace_id : null,
      label: typeof token.label === 'string' ? token.label : null,
      created_at: typeof token.created_at === 'string' ? token.created_at : null,
      revoked_at: typeof token.revoked_at === 'string' ? token.revoked_at : null,
    }));
}

export function workspaceGenerationMatches(context, workspaceId, loadId) {
  return Boolean(
    context
    && typeof context.workspaceId === 'string'
    && context.workspaceId
    && context.workspaceId === workspaceId
    && Number.isInteger(context.loadId)
    && context.loadId === loadId,
  );
}

export function scimCredentialBelongsToWorkspace(credential, workspaceId) {
  return Boolean(
    credential
    && typeof credential === 'object'
    && typeof workspaceId === 'string'
    && workspaceId
    && credential.workspace_id === workspaceId
    && typeof credential.id === 'string'
    && credential.id.trim()
    && typeof credential.token === 'string'
    && credential.token.trim(),
  );
}

export function normalizeAutoModRules(value) {
  const rules = Array.isArray(value?.rules) ? value.rules : [];
  return rules
    .filter((rule) => rule && typeof rule === 'object' && String(rule.id || '').trim())
    .map((rule) => ({
      id: String(rule.id),
      pattern: typeof rule.pattern === 'string' ? rule.pattern : '',
      match_type: ['contains', 'exact', 'prefix'].includes(rule.match_type)
        ? rule.match_type
        : 'unknown',
      action: rule.action === 'block' ? 'block' : 'unknown',
    }));
}

export function sessionIdFromJwt(token) {
  try {
    const parts = String(token || '').split('.');
    if (parts.length !== 3 || !parts[1]) return null;
    const base64 = parts[1].replace(/-/g, '+').replace(/_/g, '/');
    const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, '=');
    const binary = globalThis.atob(padded);
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    const payload = JSON.parse(new TextDecoder().decode(bytes));
    return typeof payload.sid === 'string' && payload.sid ? payload.sid : null;
  } catch {
    return null;
  }
}

export function formatSecurityTime(value) {
  if (!value) return '—';
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return '—';
  return new Intl.DateTimeFormat('zh-CN', {
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(parsed);
}

export function createElement(tag, { className, text, attrs } = {}) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = String(text);
  if (attrs) {
    for (const [name, value] of Object.entries(attrs)) {
      element.setAttribute(name, String(value));
    }
  }
  return element;
}

export function errorMessage(error) {
  if (error instanceof ApiError) {
    if (error.status === 401) return '登录已失效，即将返回登录页。';
    if (error.status === 403) return '当前角色没有执行此操作的权限。';
    if (error.status === 404) return '目标不存在或已被处理。';
    if (error.status === 0) return error.message || '网络不可用，请稍后重试。';
    return error.message || `请求失败（HTTP ${error.status}）`;
  }
  return error instanceof Error ? error.message : '请求失败，请稍后重试。';
}

let reauthScheduled = false;
export function handleApiError(error, stateElement) {
  const message = errorMessage(error);
  if (stateElement) {
    stateElement.textContent = message;
    stateElement.hidden = false;
    stateElement.classList.add('error');
  }
  toast(message, 'error');
  if (error instanceof ApiError && error.status === 401 && !reauthScheduled) {
    reauthScheduled = true;
    auth.clear();
    window.setTimeout(() => window.location.reload(), 900);
  }
}

export function setState(element, text, kind = '') {
  element.textContent = text;
  element.hidden = false;
  element.classList.toggle('error', kind === 'error');
  element.classList.toggle('ok', kind === 'ok');
}

export function hideState(element) {
  element.hidden = true;
  element.classList.remove('error', 'ok');
}

export async function withBusy(button, busyText, operation) {
  const original = button.textContent;
  button.disabled = true;
  button.textContent = busyText;
  try {
    return await operation();
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

export async function copyText(value) {
  if (!value) return false;
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    return false;
  }
}

export function roleLabel(role) {
  const labels = {
    owner: 'Owner',
    admin: 'Admin',
    member: 'Member',
    guest: 'Guest',
  };
  return labels[String(role || '').toLowerCase()] || '未知角色';
}

export function securityNodes() {
  const ids = {
    workspace: 'security-workspace',
    role: 'security-role',
    refresh: 'security-refresh',
    sessionsState: 'security-sessions-state',
    sessionsList: 'security-sessions-list',
    revokeOthers: 'security-revoke-others',
    noWorkspace: 'security-no-workspace',
    workspaceSections: 'security-workspace-sections',
    twofaStatus: 'security-twofa-status',
    ownTwofa: 'security-own-twofa',
    requireTwofa: 'security-require-twofa',
    saveTwofa: 'security-save-twofa',
    openTwofa: 'security-open-twofa',
    twofaError: 'security-twofa-error',
    regionStatus: 'security-region-status',
    storageRegion: 'security-storage-region',
    saveRegion: 'security-save-region',
    regionError: 'security-region-error',
    ipStatus: 'security-ip-status',
    ipRestricted: 'security-ip-restricted',
    ipAdmin: 'security-ip-admin',
    ipState: 'security-ip-state',
    ipList: 'security-ip-list',
    ipForm: 'security-ip-form',
    scimRestricted: 'security-scim-restricted',
    scimAdmin: 'security-scim-admin',
    scimUrl: 'security-scim-url',
    scimMintForm: 'security-scim-mint-form',
    scimState: 'security-scim-state',
    scimList: 'security-scim-list',
    scimSecret: 'security-scim-secret',
    scimTokenId: 'security-scim-token-id',
    scimToken: 'security-scim-token',
    copyScim: 'security-copy-scim',
    revokeNewScim: 'security-revoke-new-scim',
    scimError: 'security-scim-error',
    autoModRestricted: 'security-auto-mod-restricted',
    autoModAdmin: 'security-auto-mod-admin',
    autoModForm: 'security-auto-mod-form',
    autoModState: 'security-auto-mod-state',
    autoModList: 'security-auto-mod-list',
    checkSaml: 'security-check-saml',
    samlState: 'security-saml-state',
    membersRestricted: 'security-members-restricted',
    membersState: 'security-members-state',
    membersList: 'security-members-list',
  };
  return Object.fromEntries(
    Object.entries(ids).map(([name, id]) => [name, document.getElementById(id)]),
  );
}
