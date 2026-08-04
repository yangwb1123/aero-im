// governance.js — workspace audit trail and bot-platform operations.
//
// This is intentionally a thin UI over existing, permission-checked endpoints:
// audit data is visible only to workspace Owner/Admin, while bots are always
// scoped to the authenticated owner. Plaintext bot tokens are held only in this
// module's transient state and cleared whenever the modal closes.

import { api, auth } from './api.js';
import { toast } from './render.js';
import {
  copyText,
  createElement,
  errorMessage,
  formatSecurityTime,
  handleApiError,
  isWorkspaceAdmin,
  normalizeWorkspaceList,
  roleLabel,
  setState,
  withBusy,
} from './security_admin_utils.js';
import {
  auditHasFilters,
  buildAuditQuery,
  csvFilename,
  deliverySummary,
  formatJson,
  normalizeObjectArray,
  oneTimeSecret,
  parseFilterObject,
  requireExternalSubscriptionScope,
  roleForParticipant,
} from './governance_utils.js';

const EVENT_TYPES = [
  'message',
  'edited',
  'deleted',
  'reaction',
  'read',
  'typing',
  'notify',
  'pin',
  'membership',
  'poll',
  'message_seen',
  'interaction',
  'call',
];

function governanceNodes() {
  const ids = {
    workspace: 'governance-workspace',
    role: 'governance-role',
    refresh: 'governance-refresh',
    auditRestricted: 'governance-audit-restricted',
    auditForm: 'governance-audit-form',
    auditState: 'governance-audit-state',
    auditList: 'governance-audit-list',
    auditMore: 'governance-audit-more',
    auditReset: 'governance-audit-reset',
    auditExport: 'governance-audit-export',
    botCreate: 'governance-bot-create',
    botWorkspace: 'governance-bot-workspace',
    botState: 'governance-bot-state',
    botList: 'governance-bot-list',
    botDetail: 'governance-bot-detail',
    botDetailTitle: 'governance-bot-detail-title',
    botRotate: 'governance-bot-rotate',
    botSecret: 'governance-bot-secret',
    botToken: 'governance-bot-token',
    botCopy: 'governance-bot-copy',
    subForm: 'governance-sub-form',
    subSecret: 'governance-sub-secret',
    subSecretValue: 'governance-sub-secret-value',
    subSecretCopy: 'governance-sub-secret-copy',
    subState: 'governance-sub-state',
    subList: 'governance-sub-list',
    deliveryRefresh: 'governance-delivery-refresh',
    deliveryState: 'governance-delivery-state',
    deliveryList: 'governance-delivery-list',
  };
  return Object.fromEntries(
    Object.entries(ids).map(([key, id]) => [key, document.getElementById(id)]),
  );
}

function formValues(form) {
  return Object.fromEntries(new FormData(form).entries());
}

function appendOptions(select, workspaces, includePersonal = false) {
  select.replaceChildren();
  if (includePersonal) {
    select.appendChild(createElement('option', {
      text: '个人 Bot（不绑定工作区）',
      attrs: { value: '' },
    }));
  }
  for (const workspace of workspaces) {
    select.appendChild(createElement('option', {
      text: workspace.name || workspace.slug || workspace.id,
      attrs: { value: workspace.id },
    }));
  }
}

function downloadText(text, filename) {
  const url = URL.createObjectURL(new Blob([text], { type: 'text/csv;charset=utf-8' }));
  const link = createElement('a', { attrs: { href: url, download: filename } });
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

export function initGovernance() {
  const openButton = document.getElementById('btn-governance');
  const modal = document.getElementById('modal-governance');
  if (!openButton || !modal || modal.dataset.initialized === 'true') return;
  modal.dataset.initialized = 'true';
  const nodes = governanceNodes();
  const state = {
    workspaces: [],
    workspaceId: null,
    workspaceRole: null,
    auditEvents: [],
    auditHasMore: false,
    auditQuery: { limit: 100 },
    bots: [],
    selectedBotId: null,
    subscriptions: [],
    deliveries: [],
    botToken: null,
    subscriptionSecret: null,
    loadGeneration: 0,
  };

  function selectedBot() {
    return state.bots.find((bot) => bot.id === state.selectedBotId) || null;
  }

  function clearSecret() {
    state.botToken = null;
    nodes.botToken.textContent = '';
    nodes.botSecret.hidden = true;
    state.subscriptionSecret = null;
    nodes.subSecretValue.textContent = '';
    nodes.subSecret.hidden = true;
  }

  function close() {
    clearSecret();
    modal.hidden = true;
  }

  function renderWorkspaceRole() {
    const role = state.workspaceRole;
    nodes.role.textContent = role ? roleLabel(role) : '未解析';
    nodes.role.classList.toggle('admin', isWorkspaceAdmin(role));
    const allowed = Boolean(state.workspaceId) && isWorkspaceAdmin(role);
    nodes.auditRestricted.hidden = allowed;
    nodes.auditForm.hidden = !allowed;
    nodes.auditExport.disabled = !allowed;
    nodes.auditMore.hidden = !allowed || !state.auditHasMore
      || auditHasFilters(state.auditQuery);
    if (!allowed) {
      nodes.auditList.replaceChildren();
      nodes.auditList.hidden = true;
      setState(
        nodes.auditState,
        state.workspaceId
          ? '审计日志仅对工作区 Owner / Admin 开放。'
          : '当前账户尚未加入工作区。',
      );
    }
  }

  function renderAudit(events = state.auditEvents, append = false) {
    if (!append) nodes.auditList.replaceChildren();
    nodes.auditList.hidden = state.auditEvents.length === 0;
    if (!state.auditEvents.length) {
      setState(nodes.auditState, '当前查询没有审计事件。');
      nodes.auditMore.hidden = true;
      return;
    }
    nodes.auditState.hidden = true;
    for (const event of events) {
      const row = createElement('article', { className: 'governance-event' });
      const head = createElement('div', { className: 'governance-event-head' });
      head.append(
        createElement('strong', { text: event.action || 'unknown' }),
        createElement('time', { text: formatSecurityTime(event.created_at) }),
      );
      row.append(
        head,
        createElement('div', {
          className: 'governance-event-meta',
          text: `actor ${event.actor_id || 'system'} · target ${event.target || '—'}`,
        }),
        createElement('code', {
          className: 'governance-json',
          text: formatJson(event.detail),
        }),
      );
      nodes.auditList.appendChild(row);
    }
    nodes.auditMore.hidden = !state.auditHasMore || auditHasFilters(state.auditQuery);
  }

  async function resolveRole(workspaceId, generation) {
    if (!workspaceId) {
      state.workspaceRole = null;
      renderWorkspaceRole();
      return;
    }
    try {
      const members = await api.listWorkspaceMembers(workspaceId);
      if (generation !== state.loadGeneration || workspaceId !== state.workspaceId) return;
      state.workspaceRole = roleForParticipant(members, auth.getPid());
    } catch (error) {
      if (generation !== state.loadGeneration) return;
      state.workspaceRole = null;
      handleApiError(error, nodes.auditState);
    }
    renderWorkspaceRole();
  }

  async function loadAudit({ append = false } = {}) {
    if (!state.workspaceId || !isWorkspaceAdmin(state.workspaceRole)) return;
    const query = { ...state.auditQuery };
    if (append && !auditHasFilters(query)) {
      const cursor = state.auditEvents.at(-1)?.id;
      if (!cursor) return;
      query.before = cursor;
    }
    setState(nodes.auditState, append ? '正在读取更早事件…' : '正在读取审计日志…');
    try {
      const events = normalizeObjectArray(await api.workspaceAudit(state.workspaceId, query));
      state.auditEvents = append ? [...state.auditEvents, ...events] : events;
      state.auditHasMore = events.length >= Number(query.limit || 100);
      renderAudit(events, append);
    } catch (error) {
      handleApiError(error, nodes.auditState);
    }
  }

  function showBotSecret(token) {
    state.botToken = String(token || '');
    nodes.botToken.textContent = state.botToken;
    nodes.botSecret.hidden = !state.botToken;
  }

  function showSubscriptionSecret(response) {
    state.subscriptionSecret = oneTimeSecret(response);
    nodes.subSecretValue.textContent = state.subscriptionSecret || '';
    nodes.subSecret.hidden = !state.subscriptionSecret;
  }

  function renderBots() {
    nodes.botList.replaceChildren();
    nodes.botList.hidden = state.bots.length === 0;
    if (!state.bots.length) {
      setState(nodes.botState, '尚未创建 Bot。');
      nodes.botDetail.hidden = true;
      return;
    }
    nodes.botState.hidden = true;
    for (const bot of state.bots) {
      const button = createElement('button', {
        className: `governance-bot-row${bot.id === state.selectedBotId ? ' active' : ''}`,
        attrs: { type: 'button', 'data-bot-id': bot.id },
      });
      button.append(
        createElement('strong', { text: bot.name || bot.id }),
        createElement('span', {
          text: bot.workspace_id ? `工作区 ${bot.workspace_id}` : '个人 Bot',
        }),
        createElement('span', {
          text: `${bot.has_token ? '令牌已启用' : '无令牌'} · ${formatSecurityTime(bot.created_at)}`,
        }),
      );
      nodes.botList.appendChild(button);
    }
  }

  function renderSubscriptions() {
    nodes.subList.replaceChildren();
    nodes.subList.hidden = state.subscriptions.length === 0;
    if (!state.subscriptions.length) {
      setState(nodes.subState, '此 Bot 尚无事件订阅。');
      return;
    }
    nodes.subState.hidden = true;
    for (const subscription of state.subscriptions) {
      const row = createElement('div', { className: 'security-row' });
      const main = createElement('div', { className: 'security-row-main' });
      main.append(
        createElement('div', {
          className: 'security-row-title',
          text: subscription.event_type || 'unknown',
        }),
        createElement('div', {
          className: 'security-row-meta',
          text: subscription.webhook_url || '无外部 webhook（仅保留订阅记录）',
        }),
        createElement('code', {
          className: 'governance-json',
          text: formatJson(subscription.filters),
        }),
      );
      const actions = createElement('div', { className: 'security-actions' });
      if (subscription.webhook_url) {
        actions.appendChild(createElement('button', {
          className: 'btn-ghost',
          text: '轮换 Secret',
          attrs: {
            type: 'button',
            'data-subscription-secret-id': subscription.id,
          },
        }));
      }
      actions.appendChild(createElement('button', {
        className: 'btn-danger',
        text: '删除',
        attrs: {
          type: 'button',
          'data-subscription-delete-id': subscription.id,
        },
      }));
      row.append(main, actions);
      nodes.subList.appendChild(row);
    }
  }

  function renderDeliveries() {
    nodes.deliveryList.replaceChildren();
    nodes.deliveryList.hidden = state.deliveries.length === 0;
    if (!state.deliveries.length) {
      setState(nodes.deliveryState, '暂无 webhook 投递记录。');
      return;
    }
    nodes.deliveryState.hidden = true;
    for (const delivery of state.deliveries) {
      const row = createElement('div', { className: 'security-row' });
      const main = createElement('div', { className: 'security-row-main' });
      const title = createElement('div', {
        className: 'security-row-title',
        text: delivery.event_type || 'unknown',
      });
      title.appendChild(createElement('span', {
        className: `security-pill ${delivery.status === 'delivered' ? 'ok' : ''}`,
        text: delivery.status || 'unknown',
      }));
      main.append(
        title,
        createElement('div', {
          className: 'security-row-meta',
          text: `${deliverySummary(delivery)} · ${formatSecurityTime(delivery.created_at)}`,
        }),
      );
      if (delivery.error) {
        main.appendChild(createElement('code', {
          className: 'governance-json error',
          text: String(delivery.error),
        }));
      }
      row.appendChild(main);
      if (delivery.status === 'dead') {
        row.appendChild(createElement('button', {
          className: 'btn-ghost',
          text: '重新入队',
          attrs: {
            type: 'button',
            'data-delivery-requeue-id': delivery.id,
          },
        }));
      }
      nodes.deliveryList.appendChild(row);
    }
  }

  async function loadBotDetails(botId) {
    state.selectedBotId = botId;
    clearSecret();
    renderBots();
    const bot = selectedBot();
    if (!bot) {
      nodes.botDetail.hidden = true;
      return;
    }
    nodes.botDetail.hidden = false;
    nodes.botDetailTitle.textContent = bot.name || bot.id;
    setState(nodes.subState, '正在读取订阅…');
    setState(nodes.deliveryState, '正在读取投递记录…');
    const [subscriptions, deliveries] = await Promise.allSettled([
      api.listBotSubscriptions(bot.id),
      api.listBotDeliveries(bot.id),
    ]);
    if (subscriptions.status === 'fulfilled') {
      state.subscriptions = normalizeObjectArray(subscriptions.value);
      renderSubscriptions();
    } else {
      handleApiError(subscriptions.reason, nodes.subState);
    }
    if (deliveries.status === 'fulfilled') {
      state.deliveries = normalizeObjectArray(deliveries.value);
      renderDeliveries();
    } else {
      handleApiError(deliveries.reason, nodes.deliveryState);
    }
  }

  async function loadBots({ preserveSelection = true } = {}) {
    setState(nodes.botState, '正在读取 Bot…');
    try {
      state.bots = normalizeObjectArray(await api.listBots());
      const stillExists = state.bots.some((bot) => bot.id === state.selectedBotId);
      if (!preserveSelection || !stillExists) state.selectedBotId = state.bots[0]?.id || null;
      renderBots();
      if (state.selectedBotId) await loadBotDetails(state.selectedBotId);
    } catch (error) {
      handleApiError(error, nodes.botState);
    }
  }

  async function selectWorkspace(workspaceId) {
    state.workspaceId = workspaceId || null;
    state.workspaceRole = null;
    state.auditEvents = [];
    state.auditHasMore = false;
    state.auditQuery = { limit: 100 };
    nodes.auditForm.reset();
    const generation = ++state.loadGeneration;
    renderWorkspaceRole();
    await resolveRole(state.workspaceId, generation);
    if (generation === state.loadGeneration && isWorkspaceAdmin(state.workspaceRole)) {
      await loadAudit();
    }
  }

  async function loadAll() {
    clearSecret();
    const generation = ++state.loadGeneration;
    setState(nodes.auditState, '正在读取工作区…');
    try {
      state.workspaces = normalizeWorkspaceList(await api.listWorkspaces());
      appendOptions(nodes.workspace, state.workspaces);
      appendOptions(nodes.botWorkspace, state.workspaces, true);
      const previous = state.workspaces.some((workspace) => workspace.id === state.workspaceId)
        ? state.workspaceId
        : state.workspaces[0]?.id || null;
      state.workspaceId = previous;
      if (previous) nodes.workspace.value = previous;
      renderWorkspaceRole();
      await Promise.all([
        resolveRole(previous, generation).then(() => {
          if (generation === state.loadGeneration && isWorkspaceAdmin(state.workspaceRole)) {
            return loadAudit();
          }
          return undefined;
        }),
        loadBots(),
      ]);
    } catch (error) {
      handleApiError(error, nodes.auditState);
      await loadBots();
    }
  }

  openButton.addEventListener('click', () => {
    modal.hidden = false;
    loadAll();
  });
  for (const button of modal.querySelectorAll('[data-governance-close]')) {
    button.addEventListener('click', close);
  }
  modal.addEventListener('click', (event) => {
    if (event.target === modal) close();
  });
  nodes.refresh.addEventListener('click', () => loadAll());
  nodes.workspace.addEventListener('change', () => selectWorkspace(nodes.workspace.value));

  nodes.auditForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    try {
      state.auditQuery = buildAuditQuery(formValues(nodes.auditForm));
      await loadAudit();
    } catch (error) {
      setState(nodes.auditState, errorMessage(error), 'error');
    }
  });
  nodes.auditReset.addEventListener('click', async () => {
    nodes.auditForm.reset();
    state.auditQuery = { limit: 100 };
    await loadAudit();
  });
  nodes.auditMore.addEventListener('click', () => loadAudit({ append: true }));
  nodes.auditExport.addEventListener('click', async () => {
    if (!state.workspaceId || !isWorkspaceAdmin(state.workspaceRole)) return;
    try {
      const csv = await withBusy(nodes.auditExport, '导出中…', () => (
        api.workspaceAuditCsv(state.workspaceId, state.auditQuery)
      ));
      downloadText(String(csv || ''), csvFilename(state.workspaceId));
      toast('审计 CSV 已下载', 'success');
    } catch (error) {
      handleApiError(error, nodes.auditState);
    }
  });

  nodes.botCreate.addEventListener('submit', async (event) => {
    event.preventDefault();
    const values = formValues(nodes.botCreate);
    const submit = nodes.botCreate.querySelector('button[type="submit"]');
    try {
      const result = await withBusy(submit, '创建中…', () => api.createBot({
        name: String(values.name || '').trim(),
        icon_url: String(values.icon_url || '').trim() || undefined,
        workspace_id: String(values.workspace_id || '').trim() || undefined,
      }));
      nodes.botCreate.reset();
      state.selectedBotId = result?.bot_id || null;
      showBotSecret(result?.token);
      toast('Bot 已创建；令牌只显示这一次', 'success', 6000);
      await loadBots();
      showBotSecret(result?.token);
    } catch (error) {
      handleApiError(error, nodes.botState);
    }
  });
  nodes.botList.addEventListener('click', (event) => {
    const button = event.target.closest('[data-bot-id]');
    if (button) loadBotDetails(button.dataset.botId);
  });
  nodes.botRotate.addEventListener('click', async () => {
    const bot = selectedBot();
    if (!bot || !window.confirm(`轮换 ${bot.name} 的令牌？旧令牌会立即失效。`)) return;
    try {
      const result = await withBusy(nodes.botRotate, '轮换中…', () => api.rotateBotToken(bot.id));
      showBotSecret(result?.token);
      toast('Bot 令牌已轮换，请立即复制', 'success', 6000);
    } catch (error) {
      handleApiError(error, nodes.botState);
    }
  });
  nodes.botCopy.addEventListener('click', async () => {
    const copied = await copyText(state.botToken);
    toast(copied ? 'Bot 令牌已复制' : '复制失败，请手动选择明文', copied ? 'success' : 'error');
  });

  nodes.subForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    const bot = selectedBot();
    if (!bot) return;
    const values = formValues(nodes.subForm);
    const submit = nodes.subForm.querySelector('button[type="submit"]');
    try {
      const filters = parseFilterObject(values.filters);
      const webhookUrl = String(values.webhook_url || '').trim();
      requireExternalSubscriptionScope(webhookUrl, filters);
      const result = await withBusy(submit, '添加中…', () => api.createBotSubscription(bot.id, {
        event_type: values.event_type,
        filters,
        webhook_url: webhookUrl || undefined,
      }));
      nodes.subForm.reset();
      nodes.subForm.elements.event_type.value = EVENT_TYPES[0];
      await loadBotDetails(bot.id);
      showSubscriptionSecret(result);
      toast(
        oneTimeSecret(result) ? '事件订阅已添加；签名 Secret 只显示这一次' : '事件订阅已添加',
        'success',
        oneTimeSecret(result) ? 6000 : 3000,
      );
    } catch (error) {
      setState(nodes.subState, errorMessage(error), 'error');
      toast(errorMessage(error), 'error');
    }
  });
  nodes.subList.addEventListener('click', async (event) => {
    const bot = selectedBot();
    if (!bot) return;
    const rotate = event.target.closest('[data-subscription-secret-id]');
    if (rotate) {
      if (!window.confirm('轮换这个订阅的签名 Secret？旧 Secret 会立即失效。')) return;
      try {
        const result = await withBusy(rotate, '轮换中…', () => (
          api.rotateBotSubscriptionSecret(bot.id, rotate.dataset.subscriptionSecretId)
        ));
        showSubscriptionSecret(result);
        toast('签名 Secret 已轮换，请立即复制', 'success', 6000);
      } catch (error) {
        handleApiError(error, nodes.subState);
      }
      return;
    }
    const button = event.target.closest('[data-subscription-delete-id]');
    if (!button) return;
    if (!window.confirm('删除这个事件订阅？')) return;
    try {
      await withBusy(button, '删除中…', () => (
        api.deleteBotSubscription(bot.id, button.dataset.subscriptionDeleteId)
      ));
      await loadBotDetails(bot.id);
    } catch (error) {
      handleApiError(error, nodes.subState);
    }
  });
  nodes.subSecretCopy.addEventListener('click', async () => {
    const copied = await copyText(state.subscriptionSecret);
    toast(copied ? '签名 Secret 已复制' : '复制失败，请手动选择明文', copied ? 'success' : 'error');
  });
  nodes.deliveryList.addEventListener('click', async (event) => {
    const button = event.target.closest('[data-delivery-requeue-id]');
    const bot = selectedBot();
    if (!button || !bot || !window.confirm('将这条死信重新加入投递队列？')) return;
    try {
      await withBusy(button, '入队中…', () => (
        api.requeueBotDelivery(bot.id, button.dataset.deliveryRequeueId)
      ));
      await loadBotDetails(bot.id);
      toast('死信已重新入队，重试次数从 0 重新计算', 'success');
    } catch (error) {
      handleApiError(error, nodes.deliveryState);
    }
  });
  nodes.deliveryRefresh.addEventListener('click', async () => {
    const bot = selectedBot();
    if (bot) await loadBotDetails(bot.id);
  });

  const eventSelect = nodes.subForm.elements.event_type;
  for (const eventType of EVENT_TYPES) {
    eventSelect.appendChild(createElement('option', {
      text: eventType,
      attrs: { value: eventType },
    }));
  }
}

if (typeof document !== 'undefined') initGovernance();
