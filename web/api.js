// api.js — thin HTTP wrapper around the Rust backend
// All endpoints are relative; expect the static page to be served from same origin
// (or use a reverse proxy that passes /api/* to the backend).

const TOKEN_KEY = 'aero_token';
const REFRESH_KEY = 'aero_refresh';
const PID_KEY = 'aero_pid';

export const auth = {
  getToken() { return localStorage.getItem(TOKEN_KEY); },
  getRefresh() { return localStorage.getItem(REFRESH_KEY); },
  getPid() { return localStorage.getItem(PID_KEY); },
  setSession(access, refresh, pid) {
    if (access) localStorage.setItem(TOKEN_KEY, access);
    if (refresh) localStorage.setItem(REFRESH_KEY, refresh);
    if (pid) localStorage.setItem(PID_KEY, pid);
  },
  clear() {
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(REFRESH_KEY);
    localStorage.removeItem(PID_KEY);
  },
};

export class ApiError extends Error {
  constructor(status, body, message) {
    super(message || `HTTP ${status}`);
    this.status = status;
    this.body = body;
  }
}

async function request(method, path, { body, query, withAuth = true } = {}) {
  const headers = { 'Accept': 'application/json' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (withAuth) {
    const t = auth.getToken();
    if (t) headers['Authorization'] = `Bearer ${t}`;
  }
  let url = path;
  if (query && Object.keys(query).length) {
    const usp = new URLSearchParams();
    for (const [k, v] of Object.entries(query)) {
      if (v === undefined || v === null || v === '') continue;
      usp.set(k, String(v));
    }
    const qs = usp.toString();
    if (qs) url += (path.includes('?') ? '&' : '?') + qs;
  }
  let resp;
  try {
    resp = await fetch(url, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new ApiError(0, null, `网络错误:${e.message}`);
  }
  if (resp.status === 204) return null;
  const ct = resp.headers.get('content-type') || '';
  const data = ct.includes('application/json') ? await resp.json().catch(() => null) : await resp.text();
  if (!resp.ok) {
    const msg = (data && typeof data === 'object' && (data.message || data.error || data.msg)) || `HTTP ${resp.status}`;
    throw new ApiError(resp.status, data, msg);
  }
  return data;
}

export const api = {
  register({ email, password, display_name }) {
    return request('POST', '/api/auth/register', {
      body: { email, password, display_name },
      withAuth: false,
    });
  },
  login({ email, password }) {
    return request('POST', '/api/auth/login', {
      body: { email, password },
      withAuth: false,
    });
  },
  me() {
    return request('GET', '/api/me');
  },
  createRoom({ kind, name }) {
    const body = { kind };
    if (name && name.trim()) body.name = name.trim();
    return request('POST', '/api/rooms', { body });
  },
  addMember(roomId, participantId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/members`, {
      body: { participant_id: participantId },
    });
  },
  listMessages(roomId, { before, limit = 100 } = {}) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/messages`, {
      query: { before, limit },
    });
  },
};
