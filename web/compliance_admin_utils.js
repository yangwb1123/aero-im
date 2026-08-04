// Pure normalization and validation helpers for the compliance administration UI.

export function objectList(value, key) {
  const source = Array.isArray(value) ? value : value?.[key];
  return Array.isArray(source)
    ? source.filter((item) => item && typeof item === 'object')
    : [];
}

export function roleForParticipant(members, participantId) {
  const member = objectList(members).find(
    (item) => String(item.participant_id || '') === String(participantId || ''),
  );
  return String(member?.role || '').toLowerCase() || null;
}

export function complianceAccess(role) {
  const normalized = String(role || '').toLowerCase();
  return {
    admin: normalized === 'admin' || normalized === 'owner',
    owner: normalized === 'owner',
  };
}

export function parseRetentionDays(value) {
  const text = String(value ?? '').trim();
  if (!text) return null;
  if (!/^[0-9]+$/.test(text)) {
    throw new TypeError('留存天数必须是 1–3650 的整数，留空表示继承/永久保留');
  }
  const days = Number(text);
  if (!Number.isSafeInteger(days) || days < 1 || days > 3650) {
    throw new TypeError('留存天数必须是 1–3650 的整数，留空表示继承/永久保留');
  }
  return days;
}

export function buildInvitationPayload(values = {}) {
  const role = String(values.role || 'member').toLowerCase();
  if (!['guest', 'member', 'admin', 'owner'].includes(role)) {
    throw new TypeError('邀请角色无效');
  }
  const payload = { role };
  const email = String(values.email || '').trim();
  if (email) payload.email = email;

  const maxUses = String(values.max_uses ?? '').trim();
  if (maxUses) {
    if (!/^[0-9]+$/.test(maxUses) || Number(maxUses) < 1) {
      throw new TypeError('最大使用次数必须是正整数');
    }
    payload.max_uses = Number(maxUses);
  }

  const expiresDays = String(values.expires_days ?? '').trim();
  if (expiresDays) {
    const days = Number(expiresDays);
    if (!/^[0-9]+$/.test(expiresDays) || days < 1 || days > 365) {
      throw new TypeError('有效期必须是 1–365 天');
    }
    payload.expires_in_secs = days * 86_400;
  }
  return payload;
}

export function parseWebhookEvents(value) {
  return [...new Set(
    String(value || '')
      .split(/[\s,]+/)
      .map((item) => item.trim())
      .filter(Boolean),
  )];
}

export function deleteConfirmationMatches(workspace, value) {
  const typed = String(value || '').trim();
  if (!typed || !workspace) return false;
  return typed === String(workspace.name || '') || typed === String(workspace.slug || '');
}

export function oneTimeCredential(response, type) {
  if (!response || typeof response !== 'object') return null;
  if (type === 'invitation' && typeof response.token === 'string' && response.token) {
    return {
      label: '邀请链接（只显示一次）',
      value: String(response.invite_url || response.token),
    };
  }
  if (type === 'incoming' && typeof response.token === 'string' && response.token) {
    return {
      label: 'Incoming Webhook URL（只显示一次）',
      value: String(response.url || response.token),
    };
  }
  if (type === 'outgoing' && typeof response.secret === 'string' && response.secret) {
    return {
      label: 'Webhook 签名 Secret（只显示一次）',
      value: response.secret,
    };
  }
  return null;
}

export function formatComplianceTime(value) {
  if (!value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '—';
  return new Intl.DateTimeFormat('zh-CN', {
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(date);
}

export function retentionSummary(view) {
  if (!view || typeof view !== 'object') return '尚未读取';
  const inherited = view.room == null;
  const effective = view.effective == null ? '永久保留' : `${view.effective} 天`;
  return inherited ? `继承工作区 · ${effective}` : `房间覆盖 · ${effective}`;
}

export function workspaceExportFilename(workspace, now = new Date()) {
  const slug = String(workspace?.slug || workspace?.id || 'workspace')
    .replace(/[^a-zA-Z0-9_-]/g, '_');
  return `aero-workspace-${slug}-${now.toISOString().slice(0, 10)}.json`;
}
