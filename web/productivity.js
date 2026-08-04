// Unified workbench for the backend productivity surfaces that otherwise had no
// reachable SPA workflow: tasks, approvals, directory/org chart, scheduled and
// recurring messages, digests, saved items, announcements, groups and join asks.

import { api } from './api.js';
import { state } from './context.js';

function el(tag, { className, text, attrs } = {}) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = String(text);
  for (const [key, value] of Object.entries(attrs || {})) node.setAttribute(key, String(value));
  return node;
}

function control(name, placeholder, type = 'text', required = false) {
  return el('input', { attrs: { name, placeholder, type, ...(required ? { required: '' } : {}) } });
}

function field(label, node) {
  const wrap = el('label', { className: 'workbench-field' });
  wrap.append(el('span', { text: label }), node);
  return wrap;
}

function form(title, fields, submitLabel, onSubmit) {
  const node = el('form', { className: 'workbench-form' });
  node.appendChild(el('h4', { text: title }));
  for (const item of fields) node.appendChild(item);
  node.appendChild(el('button', {
    className: 'btn-primary',
    text: submitLabel,
    attrs: { type: 'submit' },
  }));
  node.addEventListener('submit', async (event) => {
    event.preventDefault();
    const button = node.querySelector('[type="submit"]');
    button.disabled = true;
    try {
      await onSubmit(Object.fromEntries(new FormData(node).entries()));
      node.reset();
    } catch {
      // The operation already painted the actionable API error in the modal.
    } finally {
      button.disabled = false;
    }
  });
  return node;
}

function section(title) {
  const node = el('section', { className: 'workbench-section' });
  node.appendChild(el('h3', { text: title }));
  return node;
}

function action(label, onClick, danger = false) {
  const button = el('button', {
    className: danger ? 'btn-danger' : 'btn-ghost',
    text: label,
    attrs: { type: 'button' },
  });
  button.addEventListener('click', async () => {
    button.disabled = true;
    try {
      await onClick();
    } catch {
      // The operation already painted the actionable API error in the modal.
    } finally {
      button.disabled = false;
    }
  });
  return button;
}

function row(title, detail = '') {
  const node = el('article', { className: 'workbench-row' });
  const main = el('div', { className: 'workbench-row-main' });
  main.append(el('strong', { text: title }), el('span', { text: detail }));
  const actions = el('div', { className: 'workbench-row-actions' });
  node.append(main, actions);
  return { node, actions };
}

function textBlock(content) {
  return [{ type: 'text', content: String(content || '').trim() }];
}

function isoFromLocal(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) throw new Error('请选择有效时间');
  return date.toISOString();
}

export function initProductivity() {
  const open = document.getElementById('btn-productivity');
  const modal = document.getElementById('modal-productivity');
  const root = document.getElementById('productivity-root');
  if (!open || !modal || !root || modal.dataset.initialized === 'true') return;
  modal.dataset.initialized = 'true';
  const status = document.getElementById('productivity-status');
  const workspaceSelect = document.getElementById('productivity-workspace');
  const tabs = Array.from(modal.querySelectorAll('[data-workbench-tab]'));
  let workspaces = [];
  let active = 'tasks';

  const workspaceId = () => workspaceSelect.value || null;
  const roomId = () => state.currentRoomId;
  const showStatus = (message, error = false) => {
    status.hidden = !message;
    status.textContent = message || '';
    status.classList.toggle('error', error);
  };
  const run = async (operation, success) => {
    showStatus('处理中…');
    try {
      const value = await operation();
      showStatus(success || '');
      return value;
    } catch (error) {
      showStatus(error?.message || String(error), true);
      throw error;
    }
  };
  const refresh = () => renderActive();

  async function renderTasks() {
    const panel = section('房间任务');
    if (!roomId()) {
      panel.appendChild(el('p', { text: '请先选择一个房间。' }));
      return panel;
    }
    panel.appendChild(form('新建任务', [
      field('标题', control('title', '需要完成什么？', 'text', true)),
      field('负责人 ID（可选）', control('assignee_id', '01J…')),
      field('截止时间（可选）', control('due_at', '', 'datetime-local')),
    ], '创建', async (values) => {
      const body = {
        title: values.title,
        assignee_id: values.assignee_id || null,
        due_at: values.due_at ? isoFromLocal(values.due_at) : null,
      };
      await run(() => api.createTask(roomId(), body), '任务已创建');
      await refresh();
    }));
    const list = el('div', { className: 'workbench-list' });
    panel.appendChild(list);
    const tasks = await api.listRoomTasks(roomId());
    for (const task of Array.isArray(tasks) ? tasks : []) {
      const item = row(task.title, `${task.status} · ${task.due_at || '无截止时间'}`);
      if (task.status !== 'done') {
        item.actions.appendChild(action('完成', async () => {
          await run(() => api.updateTask(task.id, { status: 'done' }), '任务已完成');
          await refresh();
        }));
      }
      item.actions.appendChild(action('删除', async () => {
        await run(() => api.deleteTask(task.id), '任务已删除');
        await refresh();
      }, true));
      list.appendChild(item.node);
    }
    if (!list.childElementCount) list.appendChild(el('p', { text: '暂无任务。' }));
    return panel;
  }

  async function renderApprovals() {
    const panel = section('审批');
    if (!workspaceId()) {
      panel.appendChild(el('p', { text: '当前没有工作区。' }));
      return panel;
    }
    panel.appendChild(form('发起审批', [
      field('审批人 ID', control('approver_id', '01J…', 'text', true)),
      field('标题', control('title', '审批事项', 'text', true)),
      field('说明', control('details', '可选')),
    ], '提交', async (values) => {
      await run(() => api.createApproval(workspaceId(), values), '审批已发起');
      await refresh();
    }));
    for (const [direction, heading] of [['incoming', '待我处理'], ['outgoing', '我发起的']]) {
      const block = section(heading);
      const approvals = await api.listApprovals(workspaceId(), direction);
      for (const approval of Array.isArray(approvals) ? approvals : []) {
        const item = row(approval.title, `${approval.status} · ${approval.details || ''}`);
        if (direction === 'incoming' && approval.status === 'pending') {
          for (const decision of ['approve', 'deny']) {
            item.actions.appendChild(action(decision === 'approve' ? '通过' : '拒绝', async () => {
              await run(() => api.decideApproval(approval.id, decision), '审批已更新');
              await refresh();
            }, decision === 'deny'));
          }
        }
        block.appendChild(item.node);
      }
      panel.appendChild(block);
    }
    return panel;
  }

  async function renderPeople() {
    const panel = section('成员目录与组织架构');
    if (!workspaceId()) return panel;
    const search = control('q', '搜索姓名或职位');
    const peopleList = el('div', { className: 'workbench-list' });
    const loadPeople = async () => {
      peopleList.replaceChildren();
      const people = await api.listDirectory(workspaceId(), { q: search.value, limit: 100 });
      for (const person of Array.isArray(people) ? people : []) {
        const id = person.participant_id || person.id;
        const item = row(person.display_name || id, `${person.title || '未填写职位'} · ${id}`);
        item.actions.appendChild(action('查看汇报链', async () => {
          const [manager, reports, chain] = await Promise.all([
            api.getManager(workspaceId(), id),
            api.getDirectReports(workspaceId(), id),
            api.getReportingChain(workspaceId(), id),
          ]);
          showStatus(
            `经理 ${manager?.manager_id || '无'} · 直属 ${reports?.reports?.length || 0} · 上级链 ${(chain?.chain || []).join(' → ') || '无'}`,
          );
        }));
        peopleList.appendChild(item.node);
      }
    };
    search.addEventListener('input', () => loadPeople().catch(() => {}));
    panel.append(field('搜索', search), peopleList);
    panel.appendChild(form('设置汇报关系', [
      field('成员 ID', control('participant_id', '01J…', 'text', true)),
      field('经理 ID', control('manager_id', '01J…', 'text', true)),
    ], '保存', async (values) => {
      await run(
        () => api.setManager(workspaceId(), values.participant_id, values.manager_id),
        '汇报关系已保存',
      );
      await loadPeople();
    }));
    await loadPeople();
    return panel;
  }

  async function renderAutomation() {
    const panel = section('发送计划与摘要');
    if (!roomId()) {
      panel.appendChild(el('p', { text: '请选择房间后管理发送计划。' }));
      return panel;
    }
    panel.appendChild(form('定时发送', [
      field('内容', control('content', '消息内容', 'text', true)),
      field('发送时间', control('scheduled_at', '', 'datetime-local', true)),
    ], '安排发送', async (values) => {
      await run(() => api.createScheduled(roomId(), {
        blocks: textBlock(values.content),
        scheduled_at: isoFromLocal(values.scheduled_at),
      }), '已安排发送');
      await refresh();
    }));
    const cadence = el('select', { attrs: { name: 'cadence' } });
    for (const value of ['hourly', 'daily', 'weekly']) {
      cadence.appendChild(el('option', { text: value, attrs: { value } }));
    }
    panel.appendChild(form('重复消息', [
      field('内容', control('content', '重复发送内容', 'text', true)),
      field('频率', cadence),
    ], '创建重复计划', async (values) => {
      await run(() => api.createRecurring(roomId(), {
        blocks: textBlock(values.content),
        cadence: values.cadence,
      }), '重复计划已创建');
      await refresh();
    }));
    panel.appendChild(form('AI 摘要订阅', [
      field('频率', (() => {
        const select = el('select', { attrs: { name: 'frequency' } });
        for (const value of ['daily', 'weekly']) {
          select.appendChild(el('option', { text: value, attrs: { value } }));
        }
        return select;
      })()),
    ], '订阅当前房间', async (values) => {
      await run(() => api.createDigest({
        room_id: roomId(),
        frequency: values.frequency,
      }), '摘要订阅已创建');
      await refresh();
    }));
    const lists = await Promise.all([
      api.listAllScheduled(),
      api.listRecurring(roomId()),
      api.listDigests(),
    ]);
    for (const [items, label, cancel, retry] of [
      [lists[0], '定时', (id, room) => api.cancelScheduled(room, id), (id, room) => api.retryScheduled(room, id)],
      [lists[1], '重复', (id, room) => api.cancelRecurring(room, id), null],
      [lists[2], '摘要', (id) => api.deleteDigest(id), null],
    ]) {
      for (const item of Array.isArray(items) ? items : []) {
        const isDead = item.delivery_status === 'dead' || Boolean(item.dead_at);
        const isClaimed = item.delivery_status === 'claimed';
        const status = isDead ? '失败' : (isClaimed ? '投递中' : '待发送');
        const when = item.scheduled_at || item.next_run || item.next_run_at || '';
        const detail = `${item.room_id ? `${item.room_id} · ` : ''}${when} · ${status}${item.last_error ? ` · ${item.last_error}` : ''}`;
        const entry = row(`${label} · ${item.id}`, detail);
        if (isDead && retry) {
          entry.actions.appendChild(action('重试', async () => {
            await run(() => retry(item.id, item.room_id), '已重新加入发送队列');
            await refresh();
          }));
        }
        if (!isClaimed) {
          entry.actions.appendChild(action('取消', async () => {
            await run(() => cancel(item.id, item.room_id), '计划已取消');
            await refresh();
          }, true));
        }
        panel.appendChild(entry.node);
      }
    }
    return panel;
  }

  async function renderCommunity() {
    const panel = section('收藏、提醒与工作区动态');
    panel.appendChild(form('收藏消息', [
      field('Message ID', control('message_id', '01J…', 'text', true)),
      field('备注', control('note', '可选')),
    ], '收藏', async (values) => {
      await run(() => api.saveMessage(values.message_id, values.note), '消息已收藏');
      await refresh();
    }));
    panel.appendChild(form('消息提醒', [
      field('Message ID', control('message_id', '01J…', 'text', true)),
      field('多久后', control('when', '例如 2h', 'text', true)),
    ], '设置提醒', async (values) => {
      await run(() => api.remindMessage(values.message_id, values.when), '提醒已创建');
    }));
    const saved = await api.listSaved();
    for (const item of Array.isArray(saved) ? saved : []) {
      const messageId = item.message_id || item.id;
      const entry = row(item.note || `消息 ${messageId}`, item.saved_at || '');
      entry.actions.appendChild(action('移除', async () => {
        await run(() => api.unsaveMessage(messageId), '已移除收藏');
        await refresh();
      }, true));
      panel.appendChild(entry.node);
    }
    if (workspaceId()) {
      panel.appendChild(form('发布公告（管理员）', [
        field('公告内容', control('body', '面向全工作区', 'text', true)),
        field('有效秒数（可选）', control('expires_in_secs', '86400', 'number')),
      ], '发布公告', async (values) => {
        await run(() => api.createAnnouncement(workspaceId(), {
          body: values.body,
          expires_in_secs: values.expires_in_secs ? Number(values.expires_in_secs) : null,
        }), '公告已发布');
        await refresh();
      }));
      panel.appendChild(form('新建用户组', [
        field('Handle', control('handle', 'designers', 'text', true)),
        field('名称', control('name', '设计团队', 'text', true)),
      ], '创建用户组', async (values) => {
        await run(() => api.createUserGroup(workspaceId(), values), '用户组已创建');
        await refresh();
      }));
      const [announcements, groups] = await Promise.all([
        api.listAnnouncements(workspaceId()),
        api.listUserGroups(workspaceId()),
      ]);
      for (const announcement of Array.isArray(announcements) ? announcements : []) {
        const entry = row(`公告 · ${announcement.body}`, announcement.expires_at || '长期');
        entry.actions.appendChild(action('删除', async () => {
          await run(
            () => api.deleteAnnouncement(workspaceId(), announcement.id),
            '公告已删除',
          );
          await refresh();
        }, true));
        panel.appendChild(entry.node);
      }
      for (const group of Array.isArray(groups) ? groups : []) {
        panel.appendChild(row(`@${group.handle}`, `${group.name} · ${group.id}`).node);
      }
    }
    if (roomId()) {
      const joinBlock = section('频道加入申请');
      joinBlock.appendChild(form('申请加入其他频道', [
        field('Room ID', control('room_id', '01J…', 'text', true)),
      ], '提交申请', async (values) => {
        await run(() => api.requestRoomJoin(values.room_id), '加入申请已提交');
      }));
      try {
        const response = await api.listJoinRequests(roomId());
        for (const request of response?.requests || []) {
          const entry = row(request.requester_id, request.status);
          for (const decision of ['approve', 'deny']) {
            entry.actions.appendChild(action(decision === 'approve' ? '批准' : '拒绝', async () => {
              await run(() => api.decideJoinRequest(request.id, decision), '加入申请已处理');
              await refresh();
            }, decision === 'deny'));
          }
          joinBlock.appendChild(entry.node);
        }
      } catch { /* non-admin users may submit but cannot read the queue */ }
      panel.appendChild(joinBlock);
    }
    return panel;
  }

  async function renderActive() {
    root.replaceChildren(el('p', { text: '加载中…' }));
    showStatus('');
    try {
      const renderers = {
        tasks: renderTasks,
        approvals: renderApprovals,
        people: renderPeople,
        automation: renderAutomation,
        community: renderCommunity,
      };
      root.replaceChildren(await renderers[active]());
    } catch (error) {
      root.replaceChildren(el('p', { text: `加载失败：${error?.message || error}` }));
    }
  }

  async function loadContext() {
    const response = await api.listWorkspaces();
    workspaces = Array.isArray(response) ? response : response?.workspaces || [];
    workspaceSelect.replaceChildren();
    for (const workspace of workspaces) {
      workspaceSelect.appendChild(el('option', {
        text: workspace.name || workspace.slug || workspace.id,
        attrs: { value: workspace.id },
      }));
    }
    const preferred = state.rooms.get(roomId())?.workspace_id;
    if (preferred && workspaces.some((workspace) => workspace.id === preferred)) {
      workspaceSelect.value = preferred;
    }
    await renderActive();
  }

  open.addEventListener('click', async () => {
    modal.hidden = false;
    try { await loadContext(); } catch (error) { showStatus(error?.message || String(error), true); }
  });
  modal.querySelector('[data-productivity-close]').addEventListener('click', () => {
    modal.hidden = true;
  });
  workspaceSelect.addEventListener('change', renderActive);
  for (const tab of tabs) {
    tab.addEventListener('click', () => {
      active = tab.dataset.workbenchTab;
      for (const item of tabs) item.classList.toggle('active', item === tab);
      renderActive();
    });
  }
}

initProductivity();
