// Enterprise compliance administration. Every resource stays scoped to the
// selected workspace; the server remains the final authorization authority.
import { api, auth } from './api.js';
import { toast } from './render.js';
import {
  buildInvitationPayload,
  complianceAccess,
  deleteConfirmationMatches,
  formatComplianceTime,
  objectList,
  oneTimeCredential,
  parseRetentionDays,
  parseWebhookEvents,
  retentionSummary,
  roleForParticipant,
  workspaceExportFilename,
} from './compliance_admin_utils.js';
function el(tag, { text, className, attrs } = {}) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = String(text);
  if (className) node.className = className;
  for (const [name, value] of Object.entries(attrs || {})) {
    node.setAttribute(name, String(value));
  }
  return node;
}
function labeled(text, control) {
  const label = el('label', { className: 'compliance-field' });
  label.append(el('span', { text }), control);
  return label;
}

function input(name, {
  type = 'text', placeholder = '', required = false, min, max, maxLength, value = '',
} = {}) {
  const attrs = { name, type, value };
  if (placeholder) attrs.placeholder = placeholder;
  if (required) attrs.required = '';
  if (min !== undefined) attrs.min = min;
  if (max !== undefined) attrs.max = max;
  if (maxLength !== undefined) attrs.maxlength = maxLength;
  return el('input', { attrs });
}

function button(text, className = 'btn-ghost') {
  return el('button', { text, className, attrs: { type: 'button' } });
}

function section(title, description = '') {
  const node = el('section', { className: 'security-section compliance-section' });
  const head = el('div', { className: 'security-section-head' });
  const copy = el('div');
  copy.appendChild(el('h4', { text: title }));
  if (description) copy.appendChild(el('p', { text: description }));
  head.appendChild(copy);
  node.appendChild(head);
  return node;
}

function row(title, meta, actions = []) {
  const node = el('div', { className: 'security-row' });
  const main = el('div', { className: 'security-row-main' });
  main.append(
    el('div', { text: title, className: 'security-row-title' }),
    el('div', { text: meta, className: 'security-row-meta' }),
  );
  const controls = el('div', { className: 'compliance-row-actions' });
  controls.append(...actions);
  node.append(main, controls);
  return node;
}

function listOrEmpty(items, emptyText) {
  const list = el('div', { className: 'security-list compliance-list' });
  if (!items.length) {
    list.appendChild(el('p', { text: emptyText, className: 'muted compliance-empty' }));
  } else {
    list.append(...items);
  }
  return list;
}

function roomName(room) {
  return room?.name || `${room?.kind || 'room'} · ${room?.id || '—'}`;
}

function errorText(error) {
  if (error?.status === 401) return '登录已失效，请重新登录。';
  if (error?.status === 403) return '当前角色没有执行此合规操作的权限。';
  if (error?.status === 404) return '资源不存在、已处理，或不属于当前工作区。';
  return error?.message || '请求失败，请稍后重试。';
}

export function initComplianceAdmin() {
  const open = document.getElementById('btn-compliance');
  const modal = document.getElementById('modal-compliance');
  if (!open || !modal || modal.dataset.initialized === 'true') return;
  modal.dataset.initialized = 'true';

  const workspaceSelect = document.getElementById('compliance-workspace');
  const roleNode = document.getElementById('compliance-role');
  const refresh = document.getElementById('compliance-refresh');
  const status = document.getElementById('compliance-state');
  const root = document.getElementById('compliance-root');
  const tabs = [...modal.querySelectorAll('[data-compliance-tab]')];
  const secretBox = document.getElementById('compliance-secret');
  const secretLabel = document.getElementById('compliance-secret-label');
  const secretValue = document.getElementById('compliance-secret-value');
  const secretCopy = document.getElementById('compliance-secret-copy');
  const secretClear = document.getElementById('compliance-secret-clear');

  let workspaces = [];
  let workspace = null;
  let rooms = [];
  let members = [];
  let role = null;
  let activeTab = 'retention';
  let renderGeneration = 0;
  let credential = null;
  let webhookRoomId = null;
  const actionFailed = Symbol('action-failed');

  function showStatus(message, kind = '') {
    status.textContent = message;
    status.hidden = false;
    status.classList.toggle('error', kind === 'error');
    status.classList.toggle('ok', kind === 'ok');
  }

  function hideStatus() {
    status.hidden = true;
    status.classList.remove('error', 'ok');
  }

  function clearCredential() {
    credential = null;
    secretLabel.textContent = '敏感凭据只显示一次，请立即保存。';
    secretValue.textContent = '';
    secretBox.hidden = true;
  }

  function showCredential(value) {
    credential = value;
    if (!credential) return;
    secretLabel.textContent = credential.label;
    secretValue.textContent = credential.value;
    secretBox.hidden = false;
  }

  function access() {
    return complianceAccess(role);
  }

  function requireAdmin(ownerOnly = false) {
    const allowed = ownerOnly ? access().owner : access().admin;
    if (allowed) return null;
    const required = ownerOnly ? '仅工作区 Owner 可以执行此操作。' : '仅工作区 Owner / Admin 可以管理此区域。';
    return el('div', { text: required, className: 'security-state restricted' });
  }

  async function act(control, busyText, operation, success, rerender = true) {
    const original = control.textContent;
    control.disabled = true;
    control.textContent = busyText;
    try {
      const result = await operation();
      toast(success, 'success');
      if (rerender) await renderActive();
      return result;
    } catch (error) {
      const message = errorText(error);
      showStatus(message, 'error');
      toast(message, 'error');
      return actionFailed;
    } finally {
      control.disabled = false;
      control.textContent = original;
    }
  }

  function workspaceRoomsSelect(name, includeAll = false) {
    const select = el('select', { attrs: { name } });
    if (includeAll) select.appendChild(el('option', { text: '整个工作区', attrs: { value: '' } }));
    for (const room of rooms) {
      select.appendChild(el('option', {
        text: roomName(room),
        attrs: { value: room.id },
      }));
    }
    return select;
  }

  async function renderRetention() {
    const grid = el('div', { className: 'compliance-grid' });
    const wsSection = section(
      '工作区默认留存与导出',
      '留空表示永久保留；设置较短周期后，定时清扫会按策略软删除过期消息。',
    );
    const restricted = requireAdmin();
    if (restricted) {
      wsSection.appendChild(restricted);
    } else {
      const form = el('form', { className: 'compliance-form' });
      const days = input('days', { type: 'number', min: 1, max: 3650, placeholder: '例如 365；留空为永久' });
      const save = el('button', { text: '更新默认留存', className: 'btn-primary', attrs: { type: 'submit' } });
      form.append(labeled('默认天数', days), save);
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        try {
          const value = parseRetentionDays(days.value);
          const label = value == null ? '永久保留' : `${value} 天`;
          if (!window.confirm(`将 ${workspace.name} 的默认留存改为“${label}”？这会影响后续清扫。`)) return;
          await act(save, '保存中…', () => api.setWorkspaceRetention(workspace.id, value), '默认留存已更新');
        } catch (error) {
          showStatus(errorText(error), 'error');
        }
      });
      const exportButton = button('下载工作区 JSON', 'btn-ghost');
      exportButton.disabled = !access().owner;
      exportButton.title = access().owner ? '' : '完整工作区导出仅 Owner 可用';
      exportButton.addEventListener('click', async () => {
        const snapshot = await act(
          exportButton,
          '导出中…',
          () => api.exportWorkspace(workspace.id),
          '导出已生成',
          false,
        );
        if (snapshot === actionFailed) return;
        const blob = new Blob([JSON.stringify(snapshot, null, 2)], { type: 'application/json' });
        const url = URL.createObjectURL(blob);
        const anchor = el('a', {
          attrs: { href: url, download: workspaceExportFilename(workspace) },
        });
        document.body.appendChild(anchor);
        anchor.click();
        anchor.remove();
        URL.revokeObjectURL(url);
      });
      wsSection.append(form, exportButton);
      if (!access().owner) {
        wsSection.appendChild(el('p', {
          text: '完整工作区导出包含所有房间和消息，因此仅 Owner 可下载。',
          className: 'muted compliance-note',
        }));
      }
    }

    const roomSection = section(
      '房间留存覆盖',
      '查看有效策略；留空保存即可清除覆盖并继承工作区默认值。',
    );
    if (!rooms.length) {
      roomSection.appendChild(el('div', { text: '当前工作区没有可管理的房间。', className: 'security-state' }));
    } else {
      const form = el('form', { className: 'compliance-form compliance-form-wide' });
      const roomSelect = workspaceRoomsSelect('room_id');
      const days = input('days', { type: 'number', min: 1, max: 3650, placeholder: '留空 = 继承' });
      const current = el('div', { text: '正在读取…', className: 'security-state compliance-span' });
      const load = button('查看', 'btn-ghost');
      const save = el('button', { text: '保存覆盖', className: 'btn-primary', attrs: { type: 'submit' } });
      save.disabled = !access().admin;
      async function loadRetention() {
        try {
          const view = await api.roomRetention(roomSelect.value);
          current.textContent = `${retentionSummary(view)} · 工作区 ${view.workspace == null ? '永久' : `${view.workspace} 天`}`;
          current.classList.remove('error');
          days.value = view.room == null ? '' : String(view.room);
        } catch (error) {
          current.textContent = errorText(error);
          current.classList.add('error');
        }
      }
      load.addEventListener('click', loadRetention);
      roomSelect.addEventListener('change', loadRetention);
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        try {
          const value = parseRetentionDays(days.value);
          const label = value == null ? '继承工作区' : `${value} 天`;
          if (!window.confirm(`将房间“${roomName(rooms.find((item) => item.id === roomSelect.value))}”的留存设为“${label}”？`)) return;
          await act(save, '保存中…', () => api.setRoomRetention(roomSelect.value, value), '房间留存已更新', false);
          await loadRetention();
        } catch (error) {
          showStatus(errorText(error), 'error');
        }
      });
      form.append(labeled('房间', roomSelect), labeled('覆盖天数', days), load, save, current);
      roomSection.appendChild(form);
      await loadRetention();
    }
    grid.append(wsSection, roomSection);
    return grid;
  }

  async function renderHolds() {
    const content = section('法务保全', '可覆盖整个工作区或单个房间；生效期间留存清扫会跳过受保护消息。');
    const restricted = requireAdmin();
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    const form = el('form', { className: 'compliance-form compliance-form-wide' });
    const room = workspaceRoomsSelect('room_id', true);
    const reason = input('reason', { required: true, maxLength: 1024, placeholder: '案件/审计理由' });
    const create = el('button', { text: '创建保全', className: 'btn-primary', attrs: { type: 'submit' } });
    form.append(labeled('范围', room), labeled('理由', reason), create);
    content.appendChild(form);
    const response = await api.listLegalHolds(workspace.id);
    const holds = objectList(response, 'holds');
    const list = listOrEmpty(holds.map((hold) => {
      const release = button('释放', 'btn-danger');
      release.addEventListener('click', async () => {
        if (!window.confirm(`释放法务保全“${hold.reason}”？释放后消息会重新受留存清扫约束。`)) return;
        await act(release, '释放中…', () => api.releaseLegalHold(hold.id), '法务保全已释放');
      });
      const scope = hold.room_id
        ? `房间 ${roomName(rooms.find((item) => item.id === hold.room_id))}`
        : '整个工作区';
      return row(hold.reason, `${scope} · ${formatComplianceTime(hold.created_at)}`, [release]);
    }), '没有生效中的法务保全。');
    content.appendChild(list);
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      await act(create, '创建中…', () => api.createLegalHold(workspace.id, {
        room_id: room.value || undefined,
        reason: reason.value.trim(),
      }), '法务保全已创建');
    });
    return content;
  }

  async function renderBarriers() {
    const content = section('信息隔离墙', '选择同一工作区内两个用户组，阻止两组成员互相创建 DM 或共享频道。');
    const restricted = requireAdmin();
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    const [groupResponse, barrierResponse] = await Promise.all([
      api.listUserGroups(workspace.id),
      api.listInformationBarriers(workspace.id),
    ]);
    const groups = objectList(groupResponse, 'groups');
    const barriers = objectList(barrierResponse, 'barriers');
    const groupById = new Map(groups.map((group) => [group.id, group]));
    if (groups.length >= 2) {
      const form = el('form', { className: 'compliance-form' });
      const sideA = el('select', { attrs: { name: 'group_a' } });
      const sideB = el('select', { attrs: { name: 'group_b' } });
      for (const group of groups) {
        sideA.appendChild(el('option', { text: group.name || group.handle || group.id, attrs: { value: group.id } }));
        sideB.appendChild(el('option', { text: group.name || group.handle || group.id, attrs: { value: group.id } }));
      }
      sideB.selectedIndex = 1;
      const create = el('button', { text: '建立隔离', className: 'btn-primary', attrs: { type: 'submit' } });
      form.append(labeled('用户组 A', sideA), labeled('用户组 B', sideB), create);
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        if (sideA.value === sideB.value) {
          showStatus('隔离墙两侧必须是不同用户组。', 'error');
          return;
        }
        await act(create, '创建中…', () => api.createInformationBarrier(
          workspace.id,
          sideA.value,
          sideB.value,
        ), '信息隔离墙已创建');
      });
      content.appendChild(form);
    } else {
      content.appendChild(el('div', {
        text: '至少需要两个用户组。请先在“工作台 → 成员”创建用户组。',
        className: 'security-state',
      }));
    }
    content.appendChild(listOrEmpty(barriers.map((barrier) => {
      const remove = button('删除', 'btn-danger');
      const a = groupById.get(barrier.group_a);
      const b = groupById.get(barrier.group_b);
      const title = `${a?.name || barrier.group_a} ↔ ${b?.name || barrier.group_b}`;
      remove.addEventListener('click', async () => {
        if (!window.confirm(`删除隔离墙“${title}”？删除后两组成员可重新建立会话。`)) return;
        await act(remove, '删除中…', () => api.deleteInformationBarrier(barrier.id), '信息隔离墙已删除');
      });
      return row(title, `创建于 ${formatComplianceTime(barrier.created_at)}`, [remove]);
    }), '尚未配置任何信息隔离墙。'));
    return content;
  }

  async function renderMembers() {
    const content = section('成员停用与恢复', '停用会撤销成员对当前工作区房间的访问，不删除其历史内容。');
    const restricted = requireAdmin();
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    const response = await api.listDeactivatedMembers(workspace.id);
    const deactivated = objectList(response, 'members');
    const inactive = new Map(deactivated.map((item) => [item.participant_id, item]));
    const pid = auth.getPid();
    content.appendChild(listOrEmpty(members.map((member) => {
      const record = inactive.get(member.participant_id);
      const isSelf = member.participant_id === pid;
      const ownerTarget = member.role === 'owner';
      const canManageTarget = access().owner || !ownerTarget;
      const control = button(record ? '恢复' : '停用', record ? 'btn-ghost' : 'btn-danger');
      const ownerDeactivation = ownerTarget && !record;
      control.disabled = isSelf || !canManageTarget || ownerDeactivation;
      if (isSelf) control.title = '不能停用自己的账户';
      else if (ownerDeactivation) control.title = '请先转移或降级工作区 Owner';
      else if (!canManageTarget) control.title = 'Admin 不能管理 Owner';
      control.addEventListener('click', async () => {
        if (record) {
          await act(control, '恢复中…', () => api.reactivateMember(
            workspace.id,
            member.participant_id,
          ), '成员已恢复');
          return;
        }
        if (!window.confirm(`停用成员 ${member.participant_id}？其工作区访问会立即被撤销。`)) return;
        await act(control, '停用中…', () => api.deactivateMember(
          workspace.id,
          member.participant_id,
        ), '成员已停用');
      });
      const state = record
        ? `已停用 · ${formatComplianceTime(record.deactivated_at)}`
        : `活跃 · ${member.role}`;
      return row(member.participant_id, state, [control]);
    }), '当前工作区没有成员。'));
    return content;
  }

  async function renderInvitations() {
    const content = section('工作区邀请', '邀请链接与明文 token 仅在创建响应中显示一次；列表永远不包含凭据。');
    const restricted = requireAdmin();
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    const form = el('form', { className: 'compliance-form compliance-form-wide' });
    const email = input('email', { type: 'email', placeholder: '可选邮箱' });
    const roleSelect = el('select', { attrs: { name: 'role' } });
    for (const value of ['guest', 'member', 'admin', 'owner']) {
      roleSelect.appendChild(el('option', { text: value, attrs: { value } }));
    }
    roleSelect.value = 'member';
    const maxUses = input('max_uses', { type: 'number', min: 1, placeholder: '不限' });
    const expires = input('expires_days', { type: 'number', min: 1, max: 365, placeholder: '不过期' });
    const create = el('button', { text: '创建邀请', className: 'btn-primary', attrs: { type: 'submit' } });
    form.append(
      labeled('邮箱', email),
      labeled('角色', roleSelect),
      labeled('最大使用次数', maxUses),
      labeled('有效期（天）', expires),
      create,
    );
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      try {
        const payload = buildInvitationPayload(Object.fromEntries(new FormData(form)));
        const response = await act(
          create,
          '创建中…',
          () => api.createInvitation(workspace.id, payload),
          '邀请已创建；请立即保存链接',
          false,
        );
        if (response === actionFailed) return;
        showCredential(oneTimeCredential(response, 'invitation'));
        await renderActive();
      } catch (error) {
        showStatus(errorText(error), 'error');
      }
    });
    content.appendChild(form);
    const invitations = objectList(await api.listInvitations(workspace.id));
    content.appendChild(listOrEmpty(invitations.map((invitation) => {
      const revoke = button('撤销', 'btn-danger');
      revoke.disabled = Boolean(invitation.revoked_at);
      revoke.addEventListener('click', async () => {
        if (!window.confirm(`撤销邀请 ${invitation.id}？已分享的链接将立即失效。`)) return;
        await act(revoke, '撤销中…', () => api.revokeInvitation(invitation.id), '邀请已撤销');
      });
      const target = invitation.email || '开放链接';
      const statusText = invitation.revoked_at
        ? `已撤销 · ${formatComplianceTime(invitation.revoked_at)}`
        : `已用 ${invitation.use_count}/${invitation.max_uses ?? '∞'} · 到期 ${formatComplianceTime(invitation.expires_at)}`;
      return row(`${target} · ${invitation.role}`, statusText, [revoke]);
    }), '尚未创建邀请。'));
    return content;
  }

  async function renderDeliveryPanel(webhook, host) {
    host.replaceChildren(el('div', { text: '正在读取投递与死信…', className: 'security-state' }));
    try {
      const [allResponse, deadResponse] = await Promise.all([
        api.listWebhookDeliveries(webhook.id),
        api.listWebhookDeadLetters(webhook.id),
      ]);
      const deliveries = objectList(allResponse, 'deliveries');
      const dead = objectList(deadResponse, 'dead');
      const wrap = section(`投递记录 · ${webhook.label || webhook.id}`, '死信可重入统一重试队列。');
      wrap.appendChild(listOrEmpty(deliveries.map((delivery) => row(
        `${delivery.status} · ${delivery.event_id || '无 event_id'}`,
        `尝试 ${delivery.attempts} · HTTP ${delivery.last_status_code ?? '—'} · ${formatComplianceTime(delivery.updated_at)}${delivery.last_error ? ` · ${delivery.last_error}` : ''}`,
      )), '没有投递记录。'));
      if (dead.length) {
        wrap.appendChild(el('h5', { text: '死信队列' }));
        wrap.appendChild(listOrEmpty(dead.map((delivery) => {
          const requeue = button('重新入队', 'btn-primary');
          requeue.addEventListener('click', async () => {
            const result = await act(
              requeue,
              '入队中…',
              () => api.requeueWebhookDelivery(delivery.id),
              '死信已重新入队',
              false,
            );
            if (result !== actionFailed) await renderDeliveryPanel(webhook, host);
          });
          return row(delivery.event_id || delivery.id, delivery.last_error || '重试预算已耗尽', [requeue]);
        }), ''));
      }
      host.replaceChildren(wrap);
    } catch (error) {
      host.replaceChildren(el('div', { text: errorText(error), className: 'security-state error' }));
    }
  }

  async function renderWebhooks() {
    const content = section('房间 Webhook', '入站 URL 与出站 HMAC Secret 仅创建时显示；撤销后凭据立即失效。');
    const restricted = requireAdmin();
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    if (!rooms.length) {
      content.appendChild(el('div', { text: '当前工作区没有房间。', className: 'security-state' }));
      return content;
    }
    if (!rooms.some((item) => item.id === webhookRoomId)) webhookRoomId = rooms[0].id;
    const roomSelect = workspaceRoomsSelect('webhook_room');
    roomSelect.value = webhookRoomId;
    roomSelect.addEventListener('change', () => {
      webhookRoomId = roomSelect.value;
      renderActive();
    });
    content.appendChild(labeled('房间', roomSelect));

    const forms = el('div', { className: 'compliance-grid' });
    const incomingSection = section('Incoming', '外部系统持 URL 向房间发布消息。');
    const incomingForm = el('form', { className: 'compliance-form' });
    const inLabel = input('label', { maxLength: 120, placeholder: '例如 CI 通知' });
    const botName = input('bot_name', { maxLength: 64, placeholder: 'Webhook' });
    const createIncoming = el('button', { text: '创建 Incoming', className: 'btn-primary', attrs: { type: 'submit' } });
    incomingForm.append(labeled('标签', inLabel), labeled('Bot 名称', botName), createIncoming);
    incomingSection.appendChild(incomingForm);

    const outgoingSection = section('Outgoing', '按事件过滤，并用一次性 Secret 验证 HMAC 签名。');
    const outgoingForm = el('form', { className: 'compliance-form' });
    const outLabel = input('label', { maxLength: 120, placeholder: '例如 审计归档' });
    const outUrl = input('url', { type: 'url', required: true, placeholder: 'https://hooks.example.com/aero' });
    const events = input('events', { placeholder: 'message, edited；留空 = 全部' });
    const createOutgoing = el('button', { text: '创建 Outgoing', className: 'btn-primary', attrs: { type: 'submit' } });
    outgoingForm.append(labeled('标签', outLabel), labeled('HTTPS URL', outUrl), labeled('事件', events), createOutgoing);
    outgoingSection.appendChild(outgoingForm);
    forms.append(incomingSection, outgoingSection);
    content.appendChild(forms);

    incomingForm.addEventListener('submit', async (event) => {
      event.preventDefault();
      const response = await act(createIncoming, '创建中…', () => api.createIncomingWebhook(
        webhookRoomId,
        { label: inLabel.value.trim() || undefined, bot_name: botName.value.trim() || undefined },
      ), 'Incoming Webhook 已创建；请立即保存 URL', false);
      if (response === actionFailed) return;
      showCredential(oneTimeCredential(response, 'incoming'));
      await renderActive();
    });
    outgoingForm.addEventListener('submit', async (event) => {
      event.preventDefault();
      const response = await act(createOutgoing, '创建中…', () => api.createOutgoingWebhook(
        webhookRoomId,
        {
          url: outUrl.value.trim(),
          label: outLabel.value.trim() || undefined,
          events: parseWebhookEvents(events.value),
        },
      ), 'Outgoing Webhook 已创建；请立即保存 Secret', false);
      if (response === actionFailed) return;
      showCredential(oneTimeCredential(response, 'outgoing'));
      await renderActive();
    });

    const response = await api.listRoomWebhooks(webhookRoomId);
    const incoming = objectList(response, 'incoming');
    const outgoing = objectList(response, 'outgoing');
    const inventories = el('div', { className: 'compliance-grid' });
    const incomingList = section('Incoming 清单');
    incomingList.appendChild(listOrEmpty(incoming.map((hook) => {
      const revoke = button('撤销', 'btn-danger');
      revoke.disabled = hook.revoked;
      revoke.addEventListener('click', async () => {
        if (!window.confirm(`撤销 Incoming Webhook“${hook.label || hook.id}”？外部 URL 将立即失效。`)) return;
        await act(revoke, '撤销中…', () => api.revokeIncomingWebhook(hook.id), 'Incoming Webhook 已撤销');
      });
      return row(hook.label || hook.id, `${hook.revoked ? '已撤销' : '有效'} · Bot ${hook.bot_id}`, [revoke]);
    }), '没有 Incoming Webhook。'));
    const deliveryHost = el('div', { className: 'compliance-deliveries' });
    const outgoingList = section('Outgoing 清单');
    outgoingList.appendChild(listOrEmpty(outgoing.map((hook) => {
      const deliveries = button('投递 / DLQ', 'btn-ghost');
      const revoke = button('撤销', 'btn-danger');
      deliveries.disabled = hook.revoked;
      revoke.disabled = hook.revoked;
      deliveries.addEventListener('click', () => renderDeliveryPanel(hook, deliveryHost));
      revoke.addEventListener('click', async () => {
        if (!window.confirm(`撤销 Outgoing Webhook“${hook.label || hook.id}”？后续事件将停止投递。`)) return;
        await act(revoke, '撤销中…', () => api.revokeOutgoingWebhook(hook.id), 'Outgoing Webhook 已撤销');
      });
      const eventText = hook.events?.length ? hook.events.join(', ') : '全部事件';
      return row(hook.label || hook.url, `${hook.revoked ? '已撤销' : '有效'} · ${eventText}`, [deliveries, revoke]);
    }), '没有 Outgoing Webhook。'));
    inventories.append(incomingList, outgoingList);
    content.append(inventories, deliveryHost);
    return content;
  }

  async function renderDanger() {
    const content = section(
      '永久删除工作区',
      '此操作会级联删除成员、房间、消息、审计和集成数据，且无法撤销。',
    );
    const restricted = requireAdmin(true);
    if (restricted) {
      content.appendChild(restricted);
      return content;
    }
    const warning = el('div', {
      text: `请输入工作区名称“${workspace.name}”或 slug“${workspace.slug}”，再通过最终确认。`,
      className: 'security-state error',
    });
    const confirmation = input('confirmation', { placeholder: workspace.slug, required: true });
    const remove = button('永久删除工作区', 'btn-danger');
    remove.disabled = true;
    confirmation.addEventListener('input', () => {
      remove.disabled = !deleteConfirmationMatches(workspace, confirmation.value);
    });
    remove.addEventListener('click', async () => {
      if (!deleteConfirmationMatches(workspace, confirmation.value)) {
        remove.disabled = true;
        showStatus('确认文本不匹配，删除已阻止。', 'error');
        return;
      }
      if (!window.confirm(`最终确认：永久删除 ${workspace.name}（${workspace.slug}）及其全部数据？`)) return;
      const removed = await act(
        remove,
        '删除中…',
        () => api.deleteWorkspace(workspace.id),
        '工作区已永久删除',
        false,
      );
      if (removed === actionFailed) return;
      closeModal();
      window.location.reload();
    });
    content.append(warning, labeled('名称或 slug', confirmation), remove);
    return content;
  }

  async function renderActive() {
    const generation = ++renderGeneration;
    root.replaceChildren(el('div', { text: '正在读取合规数据…', className: 'security-state' }));
    hideStatus();
    if (!workspace) {
      root.replaceChildren(el('div', { text: '当前账户没有工作区。', className: 'security-state' }));
      return;
    }
    const renderers = {
      retention: renderRetention,
      holds: renderHolds,
      barriers: renderBarriers,
      members: renderMembers,
      invitations: renderInvitations,
      webhooks: renderWebhooks,
      danger: renderDanger,
    };
    try {
      const node = await renderers[activeTab]();
      if (generation === renderGeneration) root.replaceChildren(node);
    } catch (error) {
      if (generation !== renderGeneration) return;
      const message = errorText(error);
      root.replaceChildren(el('div', { text: message, className: 'security-state error' }));
      showStatus(message, 'error');
    }
  }

  async function loadWorkspace() {
    workspace = workspaces.find((item) => item.id === workspaceSelect.value) || null;
    rooms = [];
    members = [];
    role = null;
    webhookRoomId = null;
    clearCredential();
    if (!workspace) {
      roleNode.textContent = '无工作区';
      await renderActive();
      return;
    }
    showStatus('正在验证工作区权限与资源范围…');
    const [memberResult, roomResult] = await Promise.allSettled([
      api.listWorkspaceMembers(workspace.id),
      api.listRoomsForWorkspace(workspace.id),
    ]);
    if (memberResult.status === 'fulfilled') {
      members = objectList(memberResult.value, 'members');
      role = roleForParticipant(members, auth.getPid());
    }
    if (roomResult.status === 'fulfilled') rooms = objectList(roomResult.value, 'rooms');
    roleNode.textContent = role || '权限未知';
    roleNode.classList.toggle('admin', access().admin);
    if (!role) {
      showStatus('无法确认当前工作区角色，管理操作已关闭。', 'error');
    } else if (roomResult.status === 'rejected') {
      showStatus('房间上下文读取失败，房间级操作已关闭。', 'error');
    } else {
      hideStatus();
    }
    await renderActive();
  }

  async function loadWorkspaces() {
    showStatus('正在读取工作区…');
    try {
      workspaces = objectList(await api.listWorkspaces(), 'workspaces');
      const previous = workspace?.id;
      workspaceSelect.replaceChildren();
      for (const item of workspaces) {
        workspaceSelect.appendChild(el('option', {
          text: item.name || item.slug || item.id,
          attrs: { value: item.id },
        }));
      }
      if (previous && workspaces.some((item) => item.id === previous)) {
        workspaceSelect.value = previous;
      }
      workspaceSelect.disabled = !workspaces.length;
      await loadWorkspace();
    } catch (error) {
      workspaces = [];
      workspace = null;
      role = null;
      showStatus(errorText(error), 'error');
      await renderActive();
    }
  }

  function closeModal() {
    renderGeneration += 1;
    clearCredential();
    root.replaceChildren();
    rooms = [];
    members = [];
    role = null;
    modal.hidden = true;
  }

  open.addEventListener('click', async () => {
    modal.hidden = false;
    await loadWorkspaces();
  });
  for (const closer of modal.querySelectorAll('[data-compliance-close]')) {
    closer.addEventListener('click', closeModal);
  }
  workspaceSelect.addEventListener('change', loadWorkspace);
  refresh.addEventListener('click', loadWorkspaces);
  for (const tab of tabs) {
    tab.addEventListener('click', () => {
      activeTab = tab.dataset.complianceTab;
      for (const item of tabs) item.classList.toggle('active', item === tab);
      renderActive();
    });
  }
  secretClear.addEventListener('click', clearCredential);
  secretCopy.addEventListener('click', async () => {
    if (!credential) return;
    try {
      await navigator.clipboard.writeText(credential.value);
      toast('已复制；请妥善保存', 'success');
    } catch {
      toast('浏览器拒绝剪贴板访问，请手动复制', 'error');
    }
  });
}

initComplianceAdmin();
