// Snaplink browser SDK adapter.
//
// Snaplink publishes a generated TypeScript SDK (docs/sdks/typescript/client.ts)
// whose operation names map one-to-one to the HTTP API. Aero IM is a native
// ES2020 SPA, so this small runtime adapter keeps the same public operation
// names for the browser subset we use instead of introducing a bundler or
// silently reimplementing an OAuth protocol in each form handler.

export class SSOError extends Error {
  constructor(status, error, errorDescription) {
    super(errorDescription || error || `Snaplink request failed (${status})`);
    this.name = 'SSOError';
    this.status = status;
    this.error = error;
    this.errorDescription = errorDescription;
  }
}

export class SSOClient {
  constructor({ baseUrl, clientId, fetch: fetchImpl = globalThis.fetch, getAccessToken } = {}) {
    if (!baseUrl || typeof fetchImpl !== 'function') {
      throw new SSOError(0, 'invalid_request', 'Snaplink SDK configuration is incomplete');
    }
    this.baseUrl = String(baseUrl).replace(/\/+$/, '');
    this.clientId = clientId;
    this.fetchImpl = fetchImpl;
    this.getAccessToken = getAccessToken || (() => this.token);
    this.token = undefined;
  }

  get isLoggedIn() {
    return Boolean(this.token);
  }

  get accessToken() {
    return this.token;
  }

  async request(method, path, { body, query, auth = false } = {}) {
    let url = `${this.baseUrl}${path}`;
    if (query) {
      const params = new URLSearchParams();
      for (const [key, value] of Object.entries(query)) {
        if (value !== undefined && value !== null && value !== '') params.set(key, String(value));
      }
      const encoded = params.toString();
      if (encoded) url += `?${encoded}`;
    }
    const headers = { Accept: 'application/json' };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    if (auth && this.getAccessToken) {
      const token = await this.getAccessToken();
      if (token) headers.Authorization = `Bearer ${token}`;
    }
    let response;
    try {
      response = await this.fetchImpl(url, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: 'omit',
      });
    } catch (error) {
      throw new SSOError(0, 'network_error', error?.message || 'Snaplink is unreachable');
    }
    const contentType = response.headers.get('content-type') || '';
    const payload = contentType.includes('json')
      ? await response.json().catch(() => null)
      : await response.text().catch(() => '');
    if (!response.ok) {
      const detail = payload && typeof payload === 'object'
        ? (payload.error_description || payload.error || payload.message)
        : undefined;
      throw new SSOError(response.status, payload?.error, detail);
    }
    return payload;
  }

  /** Generated SDK convenience operation: direct password login. */
  async login(username, password, { clientId = this.clientId, scope = ['openid', 'profile', 'email'], extraCredential = {} } = {}) {
    if (!clientId) throw new SSOError(0, 'invalid_request', 'Snaplink client_id is required');
    const response = await this.postLogin({
      provider: 'password',
      client_id: clientId,
      scope,
      credential: { username, password, ...extraCredential },
    });
    if (response && typeof response.access_token === 'string') this.token = response.access_token;
    return response;
  }

  /** Snaplink generated SDK operation: POST /auth/login. */
  postLogin(body) {
    return this.request('POST', '/auth/login', { body });
  }

  /** Snaplink generated SDK operation: POST /auth/mfa. */
  postMFAComplete(body) {
    return this.request('POST', '/auth/mfa', { body });
  }

  /** Snaplink generated SDK operation: POST /token. */
  postToken(body) {
    return this.request('POST', '/token', { body });
  }

  /** Snaplink generated SDK operation: GET /.well-known/openid-configuration. */
  getOpenIDConfiguration() {
    return this.request('GET', '/.well-known/openid-configuration');
  }
}
