// security_admin.js — account sessions and enterprise workspace security.
//
// This module deliberately mirrors only APIs that the Rust gateway exposes.
// OIDC and SAML configuration remains deployment-managed. SCIM bearer material
// remains mint-once; only safe credential metadata is retained in UI state.

import { api, auth } from './api.js';
import { toast } from './render.js';
import {
  copyText,
  createElement,
  errorMessage,
  formatSecurityTime,
  handleApiError,
  hideState,
  isWorkspaceAdmin,
  memberSessionRevokeControl,
  normalizeAutoModRules,
  normalizeScimTokenList,
  normalizeWorkspaceList,
  roleLabel,
  scimCredentialBelongsToWorkspace,
  securityAccessForRole,
  securityNodes,
  sessionIdFromJwt,
  setState,
  withBusy,
  workspaceGenerationMatches,
} from './security_admin_utils.js';

export function initSecurityAdmin() {
  const openButton = document.getElementById('btn-security');
  const modal = document.getElementById('modal-security');
  if (!openButton || !modal || modal.dataset.initialized === 'true') return;
  modal.dataset.initialized = 'true';

  const nodes = securityNodes();

  const state = {
    workspaces: [],
    workspaceId: null,
    role: null,
    members: [],
    membersError: null,
    sessions: [],
    ownTwofa: null,
    policy: null,
    storageRegion: null,
    allowlist: null,
    scimCredential: null,
    scimCredentialContext: null,
    scimTokens: null,
    autoModRules: null,
    workspaceLoad: 0,
  };

  function selectedWorkspace() {
    return state.workspaces.find((workspace) => workspace.id === state.workspaceId) || null;
  }

  function resetScimSecret() {
    state.scimCredential = null;
    state.scimCredentialContext = null;
    nodes.scimTokenId.textContent = '';
    nodes.scimToken.textContent = '';
    nodes.scimSecret.hidden = true;
  }

  function currentWorkspaceContext() {
    return {
      workspaceId: state.workspaceId,
      loadId: state.workspaceLoad,
    };
  }

  function isCurrentWorkspaceContext(context) {
    return workspaceGenerationMatches(context, state.workspaceId, state.workspaceLoad);
  }

  function closeModal() {
    resetScimSecret();
    modal.hidden = true;
  }

  function currentSessionId() {
    return sessionIdFromJwt(auth.getRefresh());
  }

  function renderSessions() {
    nodes.sessionsList.replaceChildren();
    nodes.sessionsList.hidden = true;
    if (!state.sessions.length) {
      setState(nodes.sessionsState, '没有可管理的活跃会话。');
      nodes.revokeOthers.disabled = true;
      return;
    }

    hideState(nodes.sessionsState);
    nodes.sessionsList.hidden = false;
    const currentId = currentSessionId();
    for (const session of state.sessions) {
      const row = createElement('div', { className: 'security-row' });
      const details = createElement('div', { className: 'security-row-main' });
      const title = createElement('div', {
        className: 'security-row-title',
        text: session.user_agent || '未记录设备信息',
      });
      const isCurrent = currentId && session.id === currentId;
      if (isCurrent) {
        title.appendChild(createElement('span', {
          className: 'security-pill ok',
          text: '当前会话',
        }));
      }
      details.append(
        title,
        createElement('div', {
          className: 'security-row-meta',
          text: `最近活动 ${formatSecurityTime(session.last_seen_at)} · 凭据 ${session.token_prefix || '—'}`,
        }),
      );
      const revoke = createElement('button', {
        className: 'btn-danger',
        text: isCurrent ? '退出本设备' : '撤销',
        attrs: { type: 'button' },
      });
      revoke.addEventListener('click', async () => {
        const warning = isCurrent
          ? '撤销当前会话将立即清除本机登录状态，确定继续？'
          : '确定撤销这个登录会话？';
        if (!window.confirm(warning)) return;
        try {
          await withBusy(revoke, '撤销中…', () => api.revokeSession(session.id));
          if (isCurrent) {
            auth.clear();
            window.location.reload();
            return;
          }
          toast('会话已撤销', 'success');
          await loadSessions();
        } catch (error) {
          handleApiError(error, nodes.sessionsState);
        }
      });
      row.append(details, revoke);
      nodes.sessionsList.appendChild(row);
    }
    nodes.revokeOthers.disabled = !auth.getRefresh() || state.sessions.length < 2;
  }

  async function loadSessions() {
    setState(nodes.sessionsState, '正在读取会话…');
    nodes.sessionsList.hidden = true;
    nodes.revokeOthers.disabled = true;
    try {
      const sessions = await api.listSessions();
      state.sessions = Array.isArray(sessions) ? sessions : [];
      renderSessions();
    } catch (error) {
      state.sessions = [];
      handleApiError(error, nodes.sessionsState);
    }
  }

  function renderWorkspaceOptions(previousId) {
    nodes.workspace.replaceChildren();
    for (const workspace of state.workspaces) {
      const option = createElement('option', {
        text: workspace.name || workspace.slug || workspace.id,
        attrs: { value: workspace.id },
      });
      nodes.workspace.appendChild(option);
    }
    const canKeep = previousId && state.workspaces.some((workspace) => workspace.id === previousId);
    state.workspaceId = canKeep ? previousId : state.workspaces[0]?.id || null;
    nodes.workspace.value = state.workspaceId || '';
    nodes.workspace.disabled = state.workspaces.length === 0;
    nodes.noWorkspace.hidden = state.workspaces.length !== 0;
    nodes.workspaceSections.hidden = state.workspaces.length === 0;
    nodes.role.textContent = state.workspaces.length ? '读取角色…' : '无工作区';
  }

  function renderOwnTwofa() {
    if (!state.ownTwofa) {
      setState(nodes.ownTwofa, '无法读取个人两步验证状态。', 'error');
      return;
    }
    if (state.ownTwofa.activated) {
      setState(nodes.ownTwofa, '你的账户已启用两步验证。', 'ok');
      return;
    }
    const suffix = state.policy?.require_2fa
      ? ' 当前工作区已强制启用，完成设置前房间数据会被锁定。'
      : '';
    setState(nodes.ownTwofa, `你的账户尚未启用两步验证。${suffix}`, 'error');
  }

  function renderPolicy() {
    const access = securityAccessForRole(state.role);
    nodes.requireTwofa.disabled = !access.manageWorkspace;
    nodes.saveTwofa.disabled = !access.manageWorkspace || !state.policy;
    renderOwnTwofa();
    if (!state.policy) {
      nodes.twofaStatus.textContent = '读取失败';
      nodes.twofaStatus.classList.remove('ok');
      return;
    }
    const required = Boolean(state.policy.require_2fa);
    nodes.requireTwofa.checked = required;
    nodes.twofaStatus.textContent = required ? '已强制' : '未强制';
    nodes.twofaStatus.classList.toggle('ok', required);
    if (!access.manageWorkspace) {
      setState(nodes.twofaError, '策略可查看；仅 Owner / Admin 可以修改。');
    } else {
      hideState(nodes.twofaError);
    }
  }

  function renderStorageRegion() {
    const access = securityAccessForRole(state.role);
    nodes.storageRegion.replaceChildren();
    const available = Array.isArray(state.storageRegion?.available_regions)
      ? state.storageRegion.available_regions
      : [];
    for (const code of available) {
      nodes.storageRegion.appendChild(createElement('option', {
        text: code === 'default' ? 'default（部署默认）' : code,
        attrs: { value: code },
      }));
    }
    const current = state.storageRegion?.storage_region || 'default';
    nodes.storageRegion.value = current;
    nodes.storageRegion.disabled = !access.manageWorkspace || !state.storageRegion;
    nodes.saveRegion.disabled = !access.manageWorkspace || !state.storageRegion;
    nodes.regionStatus.textContent = state.storageRegion ? current : '读取失败';
    nodes.regionStatus.classList.toggle('ok', Boolean(state.storageRegion));
    if (!state.storageRegion) return;
    if (!access.manageWorkspace) {
      setState(nodes.regionError, '区域可查看；仅 Owner / Admin 可以修改。');
    } else {
      hideState(nodes.regionError);
    }
  }

  function renderRoleAccess(context = currentWorkspaceContext()) {
    if (!isCurrentWorkspaceContext(context)) return;
    const access = securityAccessForRole(state.role);
    nodes.role.textContent = roleLabel(state.role);
    nodes.role.classList.toggle('admin', access.manageWorkspace);

    nodes.ipRestricted.hidden = access.manageWorkspace;
    nodes.ipAdmin.hidden = !access.manageWorkspace;
    nodes.scimRestricted.hidden = access.manageWorkspace;
    nodes.scimAdmin.hidden = !access.manageWorkspace;
    nodes.autoModRestricted.hidden = access.manageWorkspace;
    nodes.autoModAdmin.hidden = !access.manageWorkspace;
    nodes.membersRestricted.hidden = access.manageWorkspace;
    if (!access.manageWorkspace) {
      nodes.ipStatus.textContent = '管理员可见';
      nodes.ipStatus.classList.remove('ok');
    }
    renderPolicy();
    renderStorageRegion();
    renderMembers(context);
  }

  function renderAllowlist(context) {
    if (!isCurrentWorkspaceContext(context)) return;
    nodes.ipList.replaceChildren();
    nodes.ipList.hidden = true;
    if (!state.allowlist) {
      nodes.ipStatus.textContent = '读取失败';
      nodes.ipStatus.classList.remove('ok');
      return;
    }
    const entries = Array.isArray(state.allowlist.entries) ? state.allowlist.entries : [];
    const enabled = entries.length > 0;
    nodes.ipStatus.textContent = enabled ? `已启用 · ${entries.length} 条` : '未启用 · 允许全部';
    nodes.ipStatus.classList.toggle('ok', enabled);
    if (!entries.length) {
      setState(nodes.ipState, '尚未配置 CIDR。添加首条记录后，授权网络限制立即启用。');
      return;
    }

    hideState(nodes.ipState);
    nodes.ipList.hidden = false;
    for (const entry of entries) {
      const row = createElement('div', { className: 'security-row' });
      const details = createElement('div', { className: 'security-row-main' });
      details.append(
        createElement('code', { className: 'security-row-title', text: entry.cidr }),
        createElement('div', {
          className: 'security-row-meta',
          text: `${entry.note || '无备注'} · ${formatSecurityTime(entry.created_at)}`,
        }),
      );
      const remove = createElement('button', {
        className: 'btn-danger',
        text: '移除',
        attrs: { type: 'button' },
      });
      remove.addEventListener('click', async () => {
        if (!isCurrentWorkspaceContext(context)) return;
        const last = entries.length === 1;
        const warning = last
          ? `移除 ${entry.cidr} 后授权网络将关闭（恢复允许全部），确定继续？`
          : `确定移除授权网络 ${entry.cidr}？`;
        if (!window.confirm(warning)) return;
        try {
          await withBusy(remove, '移除中…', () => (
            api.removeIpAllowlist(context.workspaceId, entry.cidr)
          ));
          if (!isCurrentWorkspaceContext(context)) return;
          toast(last ? '已移除，授权网络限制已关闭' : '授权网络已移除', 'success');
          await loadAllowlist(context);
        } catch (error) {
          if (!isCurrentWorkspaceContext(context)) return;
          handleApiError(error, nodes.ipState);
        }
      });
      row.append(details, remove);
      nodes.ipList.appendChild(row);
    }
  }

  async function loadAllowlist(context) {
    if (!isCurrentWorkspaceContext(context) || !isWorkspaceAdmin(state.role)) return;
    setState(nodes.ipState, '正在读取授权网络…');
    nodes.ipList.hidden = true;
    try {
      const allowlist = await api.ipAllowlist(context.workspaceId);
      if (!isCurrentWorkspaceContext(context)) return;
      state.allowlist = allowlist;
      renderAllowlist(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      state.allowlist = null;
      handleApiError(error, nodes.ipState);
      renderAllowlist(context);
    }
  }

  function renderScimTokens(context) {
    if (!isCurrentWorkspaceContext(context)) return;
    nodes.scimList.replaceChildren();
    nodes.scimList.hidden = true;
    if (!state.scimTokens) {
      setState(nodes.scimState, '凭据清单读取失败。', 'error');
      return;
    }
    if (!state.scimTokens.length) {
      setState(nodes.scimState, '尚未创建 SCIM 凭据。');
      return;
    }
    const active = state.scimTokens.filter((token) => !token.revoked_at).length;
    setState(nodes.scimState, `${active} 个有效 · ${state.scimTokens.length} 个历史凭据`, 'ok');
    nodes.scimList.hidden = false;
    for (const token of state.scimTokens) {
      const revoked = Boolean(token.revoked_at);
      const row = createElement('div', { className: 'security-row' });
      const details = createElement('div', { className: 'security-row-main' });
      const title = createElement('div', {
        className: 'security-row-title',
        text: token.label || '未命名凭据',
      });
      title.appendChild(createElement('span', {
        className: `security-pill ${revoked ? 'neutral' : 'ok'}`,
        text: revoked ? '已撤销' : '有效',
      }));
      details.append(
        title,
        createElement('div', {
          className: 'security-row-meta',
          text: `${token.id} · 创建于 ${formatSecurityTime(token.created_at)}${
            revoked ? ` · 撤销于 ${formatSecurityTime(token.revoked_at)}` : ''
          }`,
        }),
      );
      row.appendChild(details);
      if (!revoked) {
        const revoke = createElement('button', {
          className: 'btn-danger',
          text: '撤销',
          attrs: { type: 'button' },
        });
        revoke.addEventListener('click', () => revokeScimCredential(token.id, revoke, context));
        row.appendChild(revoke);
      }
      nodes.scimList.appendChild(row);
    }
  }

  async function loadScimTokens(context) {
    if (!isCurrentWorkspaceContext(context) || !isWorkspaceAdmin(state.role)) return;
    setState(nodes.scimState, '正在读取凭据…');
    nodes.scimList.hidden = true;
    try {
      const response = await api.listScimTokens(context.workspaceId);
      if (!isCurrentWorkspaceContext(context)) return;
      state.scimTokens = normalizeScimTokenList(response)
        .filter((token) => token.workspace_id === context.workspaceId);
      renderScimTokens(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      state.scimTokens = null;
      handleApiError(error, nodes.scimState);
    }
  }

  function renderAutoModRules(context) {
    if (!isCurrentWorkspaceContext(context)) return;
    nodes.autoModList.replaceChildren();
    nodes.autoModList.hidden = true;
    if (!state.autoModRules) {
      setState(nodes.autoModState, 'AutoMod 规则读取失败。', 'error');
      return;
    }
    if (!state.autoModRules.length) {
      setState(nodes.autoModState, '尚未配置 AutoMod 规则。');
      return;
    }
    setState(nodes.autoModState, `${state.autoModRules.length} 条规则正在发送和编辑路径执行`, 'ok');
    nodes.autoModList.hidden = false;
    const matchLabels = {
      contains: '包含',
      exact: '完全相等',
      prefix: '前缀',
    };
    for (const rule of state.autoModRules) {
      const row = createElement('div', { className: 'security-row' });
      const details = createElement('div', { className: 'security-row-main' });
      details.append(
        createElement('code', { className: 'security-row-title', text: rule.pattern }),
        createElement('div', {
          className: 'security-row-meta',
          text: `${matchLabels[rule.match_type] || '未知匹配'} · ${
            rule.action === 'block' ? '阻止发送' : '未知动作'
          }`,
        }),
      );
      const remove = createElement('button', {
        className: 'btn-danger',
        text: '删除',
        attrs: { type: 'button' },
      });
      remove.addEventListener('click', async () => {
        if (!isCurrentWorkspaceContext(context)) return;
        if (!window.confirm(`确定删除 AutoMod 规则“${rule.pattern}”？`)) return;
        try {
          await withBusy(remove, '删除中…', () => (
            api.deleteAutoModRule(context.workspaceId, rule.id)
          ));
          if (!isCurrentWorkspaceContext(context)) return;
          toast('AutoMod 规则已删除', 'success');
          await loadAutoModRules(context);
        } catch (error) {
          if (!isCurrentWorkspaceContext(context)) return;
          handleApiError(error, nodes.autoModState);
        }
      });
      row.append(details, remove);
      nodes.autoModList.appendChild(row);
    }
  }

  async function loadAutoModRules(context) {
    if (!isCurrentWorkspaceContext(context) || !isWorkspaceAdmin(state.role)) return;
    setState(nodes.autoModState, '正在读取 AutoMod 规则…');
    nodes.autoModList.hidden = true;
    try {
      const response = await api.listAutoModRules(context.workspaceId);
      if (!isCurrentWorkspaceContext(context)) return;
      state.autoModRules = normalizeAutoModRules(response);
      renderAutoModRules(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      state.autoModRules = null;
      handleApiError(error, nodes.autoModState);
    }
  }

  function renderMembers(context) {
    if (!isCurrentWorkspaceContext(context)) return;
    nodes.membersList.replaceChildren();
    nodes.membersList.hidden = true;
    if (state.membersError) {
      setState(nodes.membersState, state.membersError, 'error');
      return;
    }
    if (!state.members.length) {
      setState(nodes.membersState, '没有可显示的成员，或成员列表读取失败。');
      return;
    }
    hideState(nodes.membersState);
    nodes.membersList.hidden = false;
    const ownPid = auth.getPid();
    for (const member of state.members) {
      const row = createElement('div', { className: 'security-row' });
      const details = createElement('div', { className: 'security-row-main' });
      const isSelf = member.participant_id === ownPid;
      const title = createElement('div', {
        className: 'security-row-title',
        text: member.participant_id || '未知成员',
      });
      if (isSelf) {
        title.appendChild(createElement('span', {
          className: 'security-pill neutral',
          text: '你',
        }));
      }
      details.append(
        title,
        createElement('div', {
          className: 'security-row-meta',
          text: `${roleLabel(member.role)} · 加入于 ${formatSecurityTime(member.joined_at)}`,
        }),
      );
      row.appendChild(details);
      const revokeControl = memberSessionRevokeControl(state.role, member.role);
      if (revokeControl.visible) {
        const revoke = createElement('button', {
          className: 'btn-danger',
          text: '终止全部会话',
          attrs: { type: 'button' },
        });
        revoke.disabled = revokeControl.disabled;
        if (revokeControl.reason) {
          revoke.title = revokeControl.reason;
          revoke.setAttribute('aria-label', `终止全部会话：${revokeControl.reason}`);
        }
        if (!revokeControl.disabled) {
          revoke.addEventListener('click', async () => {
            if (!isCurrentWorkspaceContext(context)) return;
            const warning = isSelf
              ? '这会撤销你自己的全部会话并退出当前设备，确定继续？'
              : `确定终止成员 ${member.participant_id} 的全部登录会话？`;
            if (!window.confirm(warning)) return;
            try {
              const result = await withBusy(revoke, '终止中…', () => (
                api.revokeMemberSessions(context.workspaceId, member.participant_id)
              ));
              if (!isCurrentWorkspaceContext(context)) return;
              if (isSelf) {
                auth.clear();
                window.location.reload();
                return;
              }
              toast(`已终止 ${Number(result?.revoked || 0)} 个会话`, 'success');
            } catch (error) {
              if (!isCurrentWorkspaceContext(context)) return;
              handleApiError(error, nodes.membersState);
            }
          });
        }
        row.appendChild(revoke);
      }
      nodes.membersList.appendChild(row);
    }
  }

  async function loadWorkspace() {
    const workspace = selectedWorkspace();
    if (!workspace) return;
    state.workspaceId = workspace.id;
    const loadId = ++state.workspaceLoad;
    state.role = null;
    state.members = [];
    state.membersError = null;
    state.policy = null;
    state.storageRegion = null;
    state.allowlist = null;
    state.scimTokens = null;
    state.autoModRules = null;
    resetScimSecret();
    nodes.ipList.replaceChildren();
    nodes.ipList.hidden = true;
    nodes.scimList.replaceChildren();
    nodes.scimList.hidden = true;
    nodes.autoModList.replaceChildren();
    nodes.autoModList.hidden = true;
    nodes.membersList.replaceChildren();
    nodes.membersList.hidden = true;
    nodes.ipAdmin.hidden = true;
    nodes.ipRestricted.hidden = true;
    nodes.scimAdmin.hidden = true;
    nodes.scimRestricted.hidden = true;
    nodes.autoModAdmin.hidden = true;
    nodes.autoModRestricted.hidden = true;
    nodes.membersRestricted.hidden = true;
    nodes.ipForm.reset();
    nodes.scimMintForm.reset();
    nodes.autoModForm.reset();
    nodes.role.textContent = '读取角色…';
    nodes.ipStatus.textContent = '读取中';
    nodes.ipStatus.classList.remove('ok');
    nodes.regionStatus.textContent = '读取中';
    nodes.regionStatus.classList.remove('ok');
    setState(nodes.membersState, '正在读取成员…');
    setState(nodes.scimState, '等待管理员权限确认…');
    setState(nodes.autoModState, '等待管理员权限确认…');
    setState(nodes.ownTwofa, '正在读取个人两步验证状态…');
    hideState(nodes.twofaError);
    hideState(nodes.regionError);

    const [membersResult, policyResult, regionResult] = await Promise.allSettled([
      api.listWorkspaceMembers(workspace.id),
      api.workspaceSecurity(workspace.id),
      api.workspaceStorageRegion(workspace.id),
    ]);
    if (loadId !== state.workspaceLoad) return;

    if (membersResult.status === 'fulfilled') {
      state.members = Array.isArray(membersResult.value) ? membersResult.value : [];
      const ownPid = auth.getPid();
      state.role = state.members.find((member) => member.participant_id === ownPid)?.role || null;
    } else {
      state.membersError = errorMessage(membersResult.reason);
      handleApiError(membersResult.reason, nodes.membersState);
    }

    if (policyResult.status === 'fulfilled') {
      state.policy = policyResult.value;
    } else {
      handleApiError(policyResult.reason, nodes.twofaError);
    }
    if (regionResult.status === 'fulfilled') {
      state.storageRegion = regionResult.value;
    } else {
      handleApiError(regionResult.reason, nodes.regionError);
    }

    const context = { workspaceId: workspace.id, loadId };
    renderRoleAccess(context);
    if (isWorkspaceAdmin(state.role)) {
      await Promise.all([
        loadAllowlist(context),
        loadScimTokens(context),
        loadAutoModRules(context),
      ]);
    }
  }

  async function loadAll() {
    const previousId = state.workspaceId || nodes.workspace.value;
    nodes.refresh.disabled = true;
    setState(nodes.sessionsState, '正在读取会话…');
    try {
      const [sessionsResult, workspacesResult, twofaResult] = await Promise.allSettled([
        api.listSessions(),
        api.listWorkspaces(),
        api.twoFactorStatus(),
      ]);

      if (sessionsResult.status === 'fulfilled') {
        state.sessions = Array.isArray(sessionsResult.value) ? sessionsResult.value : [];
        renderSessions();
      } else {
        state.sessions = [];
        handleApiError(sessionsResult.reason, nodes.sessionsState);
      }

      if (twofaResult.status === 'fulfilled') {
        state.ownTwofa = twofaResult.value;
      } else {
        state.ownTwofa = null;
        handleApiError(twofaResult.reason, nodes.ownTwofa);
      }

      if (workspacesResult.status === 'fulfilled') {
        state.workspaces = normalizeWorkspaceList(workspacesResult.value);
        renderWorkspaceOptions(previousId);
        if (state.workspaceId) await loadWorkspace();
      } else {
        state.workspaces = [];
        renderWorkspaceOptions(null);
        handleApiError(workspacesResult.reason, nodes.noWorkspace);
      }
    } finally {
      nodes.refresh.disabled = false;
    }
  }

  openButton.addEventListener('click', () => {
    modal.hidden = false;
    loadAll().catch((error) => handleApiError(error, nodes.sessionsState));
  });
  modal.addEventListener('click', (event) => {
    if (event.target === modal) closeModal();
    if (event.target instanceof HTMLElement && event.target.hasAttribute('data-security-close')) {
      closeModal();
    }
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !modal.hidden) closeModal();
  });
  nodes.refresh.addEventListener('click', () => {
    loadAll().catch((error) => handleApiError(error, nodes.sessionsState));
  });
  nodes.workspace.addEventListener('change', () => {
    state.workspaceId = nodes.workspace.value || null;
    loadWorkspace().catch((error) => handleApiError(error, nodes.membersState));
  });

  nodes.openTwofa.addEventListener('click', () => {
    closeModal();
    document.getElementById('me-avatar')?.click();
  });
  nodes.saveTwofa.addEventListener('click', async () => {
    if (!state.workspaceId || !isWorkspaceAdmin(state.role)) return;
    const context = currentWorkspaceContext();
    const required = nodes.requireTwofa.checked;
    if (required && !state.ownTwofa?.activated) {
      const proceed = window.confirm(
        '你的账户尚未启用两步验证。保存后，你也会在完成 2FA 设置前无法访问该工作区房间。确定继续？',
      );
      if (!proceed) {
        nodes.requireTwofa.checked = Boolean(state.policy?.require_2fa);
        return;
      }
    }
    try {
      const result = await withBusy(nodes.saveTwofa, '保存中…', () => (
        api.setWorkspaceSecurity(context.workspaceId, required)
      ));
      if (!isCurrentWorkspaceContext(context)) return;
      state.policy = result;
      renderPolicy();
      toast(required ? '已要求工作区成员启用两步验证' : '已取消工作区两步验证强制策略', 'success');
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      nodes.requireTwofa.checked = Boolean(state.policy?.require_2fa);
      handleApiError(error, nodes.twofaError);
    }
  });
  nodes.saveRegion.addEventListener('click', async () => {
    if (!state.workspaceId || !isWorkspaceAdmin(state.role)) return;
    const context = currentWorkspaceContext();
    const previous = state.storageRegion?.storage_region || 'default';
    const requested = nodes.storageRegion.value;
    try {
      const storageRegion = await withBusy(nodes.saveRegion, '保存中…', () => (
        api.setWorkspaceStorageRegion(context.workspaceId, requested)
      ));
      if (!isCurrentWorkspaceContext(context)) return;
      state.storageRegion = storageRegion;
      renderStorageRegion();
      toast(`未来附件将写入 ${state.storageRegion.storage_region}`, 'success');
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      nodes.storageRegion.value = previous;
      handleApiError(error, nodes.regionError);
    }
  });

  nodes.revokeOthers.addEventListener('click', async () => {
    const refreshToken = auth.getRefresh();
    if (!refreshToken) {
      toast('当前登录没有可用于保留本会话的 refresh token', 'error');
      return;
    }
    if (!window.confirm('确定退出除当前设备之外的全部登录会话？')) return;
    try {
      const result = await withBusy(nodes.revokeOthers, '处理中…', () => (
        api.revokeOtherSessions(refreshToken)
      ));
      toast(`已退出 ${Number(result?.revoked_count || 0)} 个其他会话`, 'success');
      await loadSessions();
    } catch (error) {
      handleApiError(error, nodes.sessionsState);
    }
  });

  nodes.ipForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!state.workspaceId || !isWorkspaceAdmin(state.role)) return;
    const context = currentWorkspaceContext();
    const form = new FormData(nodes.ipForm);
    const cidr = String(form.get('cidr') || '').trim();
    const note = String(form.get('note') || '').trim();
    if (!cidr) return;
    const firstEntry = !state.allowlist?.entries?.length;
    if (firstEntry && !window.confirm(
      `添加 ${cidr} 后，只有白名单内 IP 才能访问此工作区（管理本白名单的接口除外）。确定启用？`,
    )) return;
    const submit = nodes.ipForm.querySelector('button[type="submit"]');
    try {
      await withBusy(submit, '添加中…', () => (
        api.addIpAllowlist(context.workspaceId, { cidr, note: note || undefined })
      ));
      if (!isCurrentWorkspaceContext(context)) return;
      nodes.ipForm.reset();
      toast(firstEntry ? '授权网络限制已启用' : '授权网络已添加', 'success');
      await loadAllowlist(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      handleApiError(error, nodes.ipState);
    }
  });

  nodes.scimMintForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!state.workspaceId || !isWorkspaceAdmin(state.role)) return;
    const context = currentWorkspaceContext();
    const form = new FormData(nodes.scimMintForm);
    const label = String(form.get('label') || '').trim();
    const submit = nodes.scimMintForm.querySelector('button[type="submit"]');
    try {
      const credential = await withBusy(submit, '创建中…', () => (
        api.mintScimToken(context.workspaceId, label || undefined)
      ));
      if (!isCurrentWorkspaceContext(context)) {
        toast('SCIM 凭据已在先前工作区创建；请切回该工作区查看并按需撤销。', 'error', 7000);
        return;
      }
      if (!scimCredentialBelongsToWorkspace(credential, context.workspaceId)) {
        throw new Error('SCIM 凭据响应与当前工作区不匹配，已拒绝显示明文');
      }
      state.scimCredential = credential;
      state.scimCredentialContext = context;
      nodes.scimTokenId.textContent = credential.id || '';
      nodes.scimToken.textContent = credential.token || '';
      nodes.scimSecret.hidden = false;
      hideState(nodes.scimError);
      toast('SCIM 凭据已创建，请立即复制', 'success', 6000);
      await loadScimTokens(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      handleApiError(error, nodes.scimError);
    }
  });

  nodes.copyScim.addEventListener('click', async () => {
    if (!isCurrentWorkspaceContext(state.scimCredentialContext)) {
      resetScimSecret();
      toast('工作区已切换，旧凭据明文已清除', 'error');
      return;
    }
    const copied = await copyText(state.scimCredential?.token);
    toast(copied ? 'Bearer 凭据已复制' : '复制失败，请手动选择明文', copied ? 'success' : 'error');
  });

  async function revokeScimCredential(tokenId, button, context) {
    const trimmed = String(tokenId || '').trim();
    if (!isCurrentWorkspaceContext(context) || !isWorkspaceAdmin(state.role)) return;
    if (!trimmed || !window.confirm(`确定撤销 SCIM 凭据 ${trimmed}？`)) return;
    try {
      await withBusy(button, '撤销中…', () => api.revokeScimToken(trimmed));
      if (!isCurrentWorkspaceContext(context)) return;
      if (state.scimCredential?.id === trimmed) resetScimSecret();
      hideState(nodes.scimError);
      toast('SCIM 凭据已撤销', 'success');
      await loadScimTokens(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      handleApiError(error, nodes.scimError);
    }
  }

  nodes.revokeNewScim.addEventListener('click', () => {
    revokeScimCredential(
      state.scimCredential?.id,
      nodes.revokeNewScim,
      state.scimCredentialContext,
    );
  });

  nodes.autoModForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!state.workspaceId || !isWorkspaceAdmin(state.role)) return;
    const context = currentWorkspaceContext();
    const form = new FormData(nodes.autoModForm);
    const pattern = String(form.get('pattern') || '').trim();
    const matchType = String(form.get('match_type') || 'contains');
    if (!pattern) return;
    const submit = nodes.autoModForm.querySelector('button[type="submit"]');
    try {
      await withBusy(submit, '添加中…', () => api.createAutoModRule(context.workspaceId, {
        pattern,
        match_type: matchType,
        action: 'block',
      }));
      if (!isCurrentWorkspaceContext(context)) return;
      nodes.autoModForm.reset();
      toast('AutoMod 规则已添加', 'success');
      await loadAutoModRules(context);
    } catch (error) {
      if (!isCurrentWorkspaceContext(context)) return;
      handleApiError(error, nodes.autoModState);
    }
  });

  nodes.checkSaml.addEventListener('click', async () => {
    setState(nodes.samlState, '正在请求 /saml/metadata…');
    try {
      const metadata = await withBusy(nodes.checkSaml, '检测中…', () => api.samlMetadata());
      if (typeof metadata !== 'string' || !metadata.includes('EntityDescriptor')) {
        throw new Error('元数据响应格式不符合预期');
      }
      setState(nodes.samlState, '元数据入口可用；这不等同于真实 IdP 登录联调完成。', 'ok');
    } catch (error) {
      setState(nodes.samlState, `元数据不可用：${errorMessage(error)}`, 'error');
    }
  });

  nodes.scimUrl.textContent = new URL('/scim/v2', window.location.origin).href;
}

if (typeof document !== 'undefined') initSecurityAdmin();
