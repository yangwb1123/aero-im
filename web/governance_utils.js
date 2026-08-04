// Pure normalization/validation helpers for governance.js.

export function normalizeObjectArray(value) {
  return Array.isArray(value)
    ? value.filter((item) => item && typeof item === 'object')
    : [];
}

export function roleForParticipant(members, participantId) {
  const member = normalizeObjectArray(members).find(
    (item) => item.participant_id === participantId,
  );
  return String(member?.role || '').toLowerCase() || null;
}

export function parseFilterObject(value) {
  const text = String(value || '').trim();
  if (!text) return {};
  const parsed = JSON.parse(text);
  if (!parsed || Array.isArray(parsed) || typeof parsed !== 'object') {
    throw new TypeError('过滤条件必须是 JSON 对象');
  }
  return parsed;
}

export function requireExternalSubscriptionScope(webhookUrl, filters) {
  if (!String(webhookUrl || '').trim()) return;
  const roomId = String(filters?.room_id || '').trim();
  const workspaceId = String(filters?.workspace_id || '').trim();
  if (!roomId && !workspaceId) {
    throw new TypeError('外部 Webhook 必须在过滤 JSON 中填写 room_id 或 workspace_id');
  }
}

export function toRfc3339(value) {
  const text = String(value || '').trim();
  if (!text) return undefined;
  const parsed = new Date(text);
  if (Number.isNaN(parsed.getTime())) {
    throw new TypeError('时间格式无效');
  }
  return parsed.toISOString();
}

export function buildAuditQuery(values = {}) {
  const query = { limit: 100 };
  for (const key of ['action', 'actor', 'target']) {
    const value = String(values[key] || '').trim();
    if (value) query[key] = value;
  }
  const after = toRfc3339(values.after);
  const until = toRfc3339(values.until);
  if (after) query.after = after;
  if (until) query.until = until;
  if (values.before) query.before = String(values.before);
  return query;
}

export function auditHasFilters(query = {}) {
  return ['action', 'actor', 'target', 'after', 'until'].some((key) => Boolean(query[key]));
}

export function formatJson(value, maxLength = 320) {
  let text;
  try {
    text = JSON.stringify(value ?? {});
  } catch {
    text = String(value ?? '');
  }
  return text.length <= maxLength ? text : `${text.slice(0, maxLength - 1)}…`;
}

export function deliverySummary(delivery) {
  const status = String(delivery?.status || 'unknown');
  const http = delivery?.http_status == null ? '' : ` · HTTP ${delivery.http_status}`;
  const attempts = Number(delivery?.attempts || 0);
  return `${status}${http} · ${attempts} 次尝试`;
}

export function oneTimeSecret(response) {
  return typeof response?.secret === 'string' && response.secret.length > 0
    ? response.secret
    : null;
}

export function csvFilename(workspaceId, now = new Date()) {
  const day = now.toISOString().slice(0, 10);
  const safeId = String(workspaceId || 'workspace').replace(/[^a-zA-Z0-9_-]/g, '_');
  return `aero-audit-${safeId}-${day}.csv`;
}
