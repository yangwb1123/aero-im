var __defProp = Object.defineProperty;
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);

// vendor/snaplink_sso_client.ts
var SSOError = class extends Error {
  constructor(status, error, errorDescription) {
    super(errorDescription || error || `sso request failed with status ${status}`);
    __publicField(this, "status", status);
    __publicField(this, "error", error);
    __publicField(this, "errorDescription", errorDescription);
    this.name = "SSOError";
  }
};
var SSOClient = class {
  constructor(opts) {
    __publicField(this, "baseUrl");
    __publicField(this, "clientId");
    __publicField(this, "fetchImpl");
    __publicField(this, "getAccessToken");
    /** Access token captured by login(); auto-attached to auth-required calls. */
    __publicField(this, "token");
    this.baseUrl = opts.baseUrl.replace(/\/+$/, "");
    this.clientId = opts.clientId;
    this.fetchImpl = opts.fetch ?? fetch;
    this.getAccessToken = opts.getAccessToken ?? (() => this.token);
  }
  /** True once login() succeeded and a token is held. */
  get isLoggedIn() {
    return !!this.token;
  }
  /** The access token captured by login() (undefined before login/after logout). */
  get accessToken() {
    return this.token;
  }
  /**
   * Password login: fills in the configured client_id and returns the auth
   * info (access_token / id_token / refresh_token / ...) directly — no redirect.
   * The token is captured internally so subsequent getUserInfo()/getMe() calls
   * auto-attach it. This is the simplest integration:
   *
   *   const sso = new SSOClient({ baseUrl, clientId: "my-app" });
   *   const auth = await sso.login(username, password);
   *   const me = await sso.getUserInfo();
   *
   * CHECK isLoggedIn (or "access_token" in auth) before assuming success: when
   * the account/client has MFA enabled, this resolves to a MFARequiredResponse
   * instead — no token is issued until POST /auth/mfa completes the second leg.
   */
  async login(username, password, opts) {
    const clientId = opts?.clientId ?? this.clientId;
    if (!clientId) {
      throw new SSOError(0, "invalid_request", "clientId is required (set it in the constructor or pass it to login)");
    }
    const resp = await this.postLogin({
      provider: "password",
      client_id: clientId,
      scope: opts?.scope ?? ["openid", "profile", "email"],
      credential: { username, password, ...opts?.extraCredential ?? {} }
    });
    if ("access_token" in resp) this.token = resp.access_token;
    return resp;
  }
  /** Clear the held token and best-effort revoke the server session. */
  async logout() {
    try {
      if (this.token) await this.postLogout({});
    } finally {
      this.token = void 0;
    }
  }
  async request(method, path, opts = {}) {
    let url = this.baseUrl + path;
    if (opts.query) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(opts.query)) {
        if (v !== void 0) qs.set(k, String(v));
      }
      const s = qs.toString();
      if (s) url += "?" + s;
    }
    const headers = { Accept: "application/json" };
    let body;
    if (opts.body !== void 0) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(opts.body);
    }
    if (opts.auth && this.getAccessToken) {
      const token = await this.getAccessToken();
      if (token) headers["Authorization"] = `Bearer ${token}`;
    }
    const res = await this.fetchImpl(url, { method, headers, body });
    if (!res.ok) {
      let error;
      let errorDescription;
      try {
        const parsed = await res.json();
        error = parsed?.error;
        errorDescription = parsed?.error_description;
      } catch {
      }
      throw new SSOError(res.status, error, errorDescription);
    }
    if (res.status === 204) return void 0;
    const text = await res.text();
    return text ? JSON.parse(text) : void 0;
  }
  // ---- admin ----
  /** List the zero-trust conditional-access (CAP) policies (governance view). */
  async listAccessPolicies() {
    return this.request("GET", `/api/v1/admin/access-policies`, { auth: true });
  }
  /** Apply current conditional-access policies to active sessions now. */
  async convergeAccessPolicySessions() {
    return this.request("POST", `/api/v1/admin/access-policies/converge`, { auth: true });
  }
  /** Clear a brute-force account lockout (helpdesk unlock). */
  async adminClearAccountLockout(body) {
    return this.request("POST", `/api/v1/admin/account-lockout/clear`, { body, auth: true });
  }
  /** Export the role-definition authorization policy bundle. */
  async getAuthzPolicyBundle(query) {
    return this.request("GET", `/api/v1/admin/authz/policy-bundle`, { query, auth: true });
  }
  /** List exhausted OIDC back-channel logout deliveries. */
  async listBackchannelLogoutFailures(query) {
    return this.request("GET", `/api/v1/admin/backchannel-logout/failures`, { query, auth: true });
  }
  /** Replay a batch of due OIDC back-channel logout failures. */
  async replayDueBackchannelLogoutFailures(query) {
    return this.request("POST", `/api/v1/admin/backchannel-logout/failures/replay`, { query, auth: true });
  }
  /** Replay one OIDC back-channel logout failure. */
  async replayBackchannelLogoutFailure(id) {
    return this.request("POST", `/api/v1/admin/backchannel-logout/failures/${encodeURIComponent(id)}/replay`, { auth: true });
  }
  /** Trigger an online VACUUM INTO backup of every registered SQLite source. */
  async triggerBackup() {
    return this.request("POST", `/api/v1/admin/backup`, { auth: true });
  }
  /** Conditionally clear tenant-specific branding. */
  async deleteAdminBranding(query) {
    return this.request("DELETE", `/api/v1/admin/branding`, { query, auth: true });
  }
  /** Get tenant branding. */
  async getAdminBranding(query) {
    return this.request("GET", `/api/v1/admin/branding`, { query, auth: true });
  }
  /** Conditionally replace tenant branding. */
  async updateAdminBranding(query, body) {
    return this.request("PUT", `/api/v1/admin/branding`, { query, body, auth: true });
  }
  /** List pending + active break-glass admin sessions. */
  async adminListBreakGlass() {
    return this.request("GET", `/api/v1/admin/break-glass`, { auth: true });
  }
  /** Create a break-glass (emergency support) admin session. */
  async adminCreateBreakGlass(body) {
    return this.request("POST", `/api/v1/admin/break-glass`, { body, auth: true });
  }
  /** Revoke a break-glass admin session. */
  async adminRevokeBreakGlass(id) {
    return this.request("DELETE", `/api/v1/admin/break-glass/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Approve a pending break-glass admin session (two-person rule). */
  async adminApproveBreakGlass(id) {
    return this.request("POST", `/api/v1/admin/break-glass/${encodeURIComponent(id)}/approve`, { auth: true });
  }
  /** Mint a live impersonation bearer for a break-glass grant. */
  async adminImpersonateBreakGlass(id) {
    return this.request("POST", `/api/v1/admin/break-glass/${encodeURIComponent(id)}/impersonate`, { auth: true });
  }
  /** List admin change requests (pending, decided, and applied). */
  async adminListChanges() {
    return this.request("GET", `/api/v1/admin/changes`, { auth: true });
  }
  /** Propose a generic admin change requiring a second admin's approval. */
  async adminProposeChange(body) {
    return this.request("POST", `/api/v1/admin/changes`, { body, auth: true });
  }
  /** Get one admin change request. */
  async adminGetChange(id) {
    return this.request("GET", `/api/v1/admin/changes/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Approve a pending admin change (two-person rule). */
  async adminApproveChange(id) {
    return this.request("POST", `/api/v1/admin/changes/${encodeURIComponent(id)}/approve`, { auth: true });
  }
  /** Reject a pending admin change. */
  async adminRejectChange(id) {
    return this.request("POST", `/api/v1/admin/changes/${encodeURIComponent(id)}/reject`, { auth: true });
  }
  /** List registered clients. */
  async adminClientList(query) {
    return this.request("GET", `/api/v1/admin/clients`, { query, auth: true });
  }
  /** Create a client. */
  async adminClientCreate(body) {
    return this.request("POST", `/api/v1/admin/clients`, { body, auth: true });
  }
  /** Delete a client. */
  async adminClientDelete(id) {
    return this.request("DELETE", `/api/v1/admin/clients/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a client (secret cleared). */
  async adminClientGet(id) {
    return this.request("GET", `/api/v1/admin/clients/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Update a client. */
  async adminClientUpdate(id, body) {
    return this.request("PUT", `/api/v1/admin/clients/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Approve a pending client registration. */
  async adminClientApprove(id) {
    return this.request("POST", `/api/v1/admin/clients/${encodeURIComponent(id)}/approve`, { auth: true });
  }
  /** Reject a pending client registration. */
  async adminClientReject(id, body) {
    return this.request("POST", `/api/v1/admin/clients/${encodeURIComponent(id)}/reject`, { body, auth: true });
  }
  /** Mint a fresh client_secret. */
  async adminClientRotateSecret(id, body) {
    return this.request("POST", `/api/v1/admin/clients/${encodeURIComponent(id)}/rotate-secret`, { body, auth: true });
  }
  /** Active OAuth consent grants, system-wide. */
  async adminComplianceActiveConsents() {
    return this.request("GET", `/api/v1/admin/compliance/consents`, { auth: true });
  }
  /** GDPR Art. 30 data map — the categories of personal data this server processes. */
  async adminComplianceDataMap() {
    return this.request("GET", `/api/v1/admin/compliance/data-map`, { auth: true });
  }
  /** Trigger one automated data-retention sweep pass on demand. */
  async adminTriggerRetentionSweep(body) {
    return this.request("POST", `/api/v1/admin/compliance/retention-sweep`, { body, auth: true });
  }
  /** SOC2 evidence pack — access review, change management, access revocation. */
  async adminSOC2Evidence(query) {
    return this.request("GET", `/api/v1/admin/compliance/soc2-evidence`, { query, auth: true });
  }
  /** Config snapshot as loaded at startup (redacted). */
  async getAppliedConfig() {
    return this.request("GET", `/api/v1/admin/config/applied`, { auth: true });
  }
  /** RFC 6902 JSON Patch from a peer cluster's config to this cluster's running config (redacted). */
  async postConfigClusterDiff(body) {
    return this.request("POST", `/api/v1/admin/config/cluster-diff`, { body, auth: true });
  }
  /** RFC 6902 JSON Patch from applied to running config (redacted). */
  async getConfigDiff() {
    return this.request("GET", `/api/v1/admin/config/diff`, { auth: true });
  }
  /** Runtime-configuration change history (config_history). */
  async listConfigHistory(query) {
    return this.request("GET", `/api/v1/admin/config/history`, { query, auth: true });
  }
  /** Current effective config snapshot (redacted). */
  async getRunningConfig() {
    return this.request("GET", `/api/v1/admin/config/running`, { auth: true });
  }
  /** List a tenant's B2B enterprise connections. */
  async adminListConnections(query) {
    return this.request("GET", `/api/v1/admin/connections`, { query, auth: true });
  }
  /** Create or replace a B2B enterprise connection. */
  async adminUpsertConnection(body) {
    return this.request("POST", `/api/v1/admin/connections`, { body, auth: true });
  }
  /** Delete a B2B enterprise connection. */
  async adminDeleteConnection(id) {
    return this.request("DELETE", `/api/v1/admin/connections/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Get a B2B enterprise connection by id. */
  async adminGetConnection(id) {
    return this.request("GET", `/api/v1/admin/connections/${encodeURIComponent(id)}`, { auth: true });
  }
  /** List a connection's email-domain ownership claims. */
  async adminListConnectionDomains(id) {
    return this.request("GET", `/api/v1/admin/connections/${encodeURIComponent(id)}/domains`, { auth: true });
  }
  /** Verify a claimed email domain via its DNS TXT challenge. */
  async adminVerifyConnectionDomain(id, domain) {
    return this.request("POST", `/api/v1/admin/connections/${encodeURIComponent(id)}/domains/${encodeURIComponent(domain)}/verify`, { auth: true });
  }
  /** Read a connection's last recorded reachability probe outcome. */
  async adminGetConnectionHealth(id) {
    return this.request("GET", `/api/v1/admin/connections/${encodeURIComponent(id)}/health`, { auth: true });
  }
  /** Synchronously test a connection's upstream reachability. */
  async adminProbeConnection(id) {
    return this.request("POST", `/api/v1/admin/connections/${encodeURIComponent(id)}/probe`, { auth: true });
  }
  /** Credential-rotation governance inventory (type, version, status, next rotation due). */
  async getCredentialInventory() {
    return this.request("GET", `/api/v1/admin/credentials`, { auth: true });
  }
  /** Emergency credential compromise-response — force-rotate a leaked credential with no overlap. */
  async adminCompromiseCredential(type, body) {
    return this.request("POST", `/api/v1/admin/credentials/${encodeURIComponent(type)}/compromise`, { body, auth: true });
  }
  /** Cryptographic material inventory (signing keys, JWE keys, KMS-backed keys, trust anchors). */
  async getCryptoKeyInventory(query) {
    return this.request("GET", `/api/v1/admin/crypto/keys`, { query, auth: true });
  }
  /** Report a catalogued cryptographic key compromised (inventory bookkeeping, not revocation). */
  async adminReportCryptoKeyCompromise(id, body) {
    return this.request("POST", `/api/v1/admin/crypto/keys/${encodeURIComponent(id)}/compromise`, { body, auth: true });
  }
  /** List devices across users. */
  async listAdminDevices() {
    return this.request("GET", `/api/v1/admin/devices`, { auth: true });
  }
  /** Revoke a filtered set of devices. */
  async bulkRevokeAdminDevices() {
    return this.request("POST", `/api/v1/admin/devices/bulk-revoke`, { auth: true });
  }
  /** Return aggregate device-security statistics. */
  async getAdminDeviceStats() {
    return this.request("GET", `/api/v1/admin/devices/stats`, { auth: true });
  }
  /** Get security activity for one device. */
  async getAdminDeviceActivity(id) {
    return this.request("GET", `/api/v1/admin/devices/${encodeURIComponent(id)}/activity`, { auth: true });
  }
  /** Reset the trust state for one device. */
  async resetAdminDeviceTrust(id) {
    return this.request("POST", `/api/v1/admin/devices/${encodeURIComponent(id)}/trust`, { auth: true });
  }
  /** Embedded, self-contained API-documentation viewer (opt-in, sso.WithAPIDocsUI). */
  async getAPIDocsUI() {
    return this.request("GET", `/api/v1/admin/docs`, { auth: true });
  }
  /** This same OpenAPI document, parsed and re-served as JSON. */
  async getAPIDocsSpec() {
    return this.request("GET", `/api/v1/admin/docs/openapi.json`, { auth: true });
  }
  /** List domains (hostname → tenant mappings). */
  async domainList(query) {
    return this.request("GET", `/api/v1/admin/domains`, { query, auth: true });
  }
  /** Create a domain. */
  async domainCreate(body) {
    return this.request("POST", `/api/v1/admin/domains`, { body, auth: true });
  }
  /** Delete a domain. */
  async domainDelete(hostname) {
    return this.request("DELETE", `/api/v1/admin/domains/${encodeURIComponent(hostname)}`, { auth: true });
  }
  /** Fetch a domain. */
  async domainGet(hostname) {
    return this.request("GET", `/api/v1/admin/domains/${encodeURIComponent(hostname)}`, { auth: true });
  }
  /** Update a domain. */
  async domainUpdate(hostname, body) {
    return this.request("PUT", `/api/v1/admin/domains/${encodeURIComponent(hostname)}`, { body, auth: true });
  }
  /** Read the current disaster-recovery degraded-service mode. */
  async getDegradationMode() {
    return this.request("GET", `/api/v1/admin/dr/mode`, { auth: true });
  }
  /** Set the disaster-recovery degraded-service mode. */
  async setDegradationMode(body) {
    return this.request("POST", `/api/v1/admin/dr/mode`, { body, auth: true });
  }
  /** Disaster-recovery readiness status. */
  async getDRStatus() {
    return this.request("GET", `/api/v1/admin/dr/status`, { auth: true });
  }
  /** Runtime endpoint inventory — every route this replica actually registered. */
  async getAdminEndpoints() {
    return this.request("GET", `/api/v1/admin/endpoints`, { auth: true });
  }
  /** Realtime admin event stream (Server-Sent Events). */
  async streamAdminEvents(query) {
    return this.request("GET", `/api/v1/admin/events/stream`, { query, auth: true });
  }
  /** Federation peer metadata-health listing (fetch success/failure + TLS cert expiry). */
  async getFederationHealth() {
    return this.request("GET", `/api/v1/admin/federation/health`, { auth: true });
  }
  /** List signing keys (public metadata). */
  async adminKeyList() {
    return this.request("GET", `/api/v1/admin/keys`, { auth: true });
  }
  /** Rotate the primary signing key on demand. */
  async adminKeyRotate(body) {
    return this.request("POST", `/api/v1/admin/keys/rotate`, { body, auth: true });
  }
  /** List LOCAL (password-authenticated) users. */
  async adminLocalUserList(query) {
    return this.request("GET", `/api/v1/admin/local-users`, { query, auth: true });
  }
  /** Create a LOCAL (password-authenticated) user. */
  async adminLocalUserCreate(body) {
    return this.request("POST", `/api/v1/admin/local-users`, { body, auth: true });
  }
  /** Delete a LOCAL user. */
  async adminLocalUserDelete(id) {
    return this.request("DELETE", `/api/v1/admin/local-users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a LOCAL user. */
  async adminLocalUserGet(id) {
    return this.request("GET", `/api/v1/admin/local-users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Update a LOCAL user's email/display name. */
  async adminLocalUserUpdate(id, body) {
    return this.request("PUT", `/api/v1/admin/local-users/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Revoke the admin bearer token used on this request. */
  async postAdminLogout() {
    return this.request("POST", `/api/v1/admin/logout`, { auth: true });
  }
  /** List durable multi-step admin operations. */
  async adminOperationList() {
    return this.request("GET", `/api/v1/admin/operations`, { auth: true });
  }
  /** Get a durable operation after reconnecting. */
  async adminOperationGet(id) {
    return this.request("GET", `/api/v1/admin/operations/${encodeURIComponent(id)}`, { auth: true });
  }
  /** List role assignments for a client. */
  async permissionListAssignments(clientId) {
    return this.request("GET", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/assignments`, { auth: true });
  }
  /** Assign roles to a user (additive). */
  async permissionAssignRoles(clientId, userId, body) {
    return this.request("POST", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/assignments/${encodeURIComponent(userId)}`, { body, auth: true });
  }
  /** Unassign roles from a user. */
  async permissionUnassignRoles(clientId, userId, body) {
    return this.request("POST", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/assignments/${encodeURIComponent(userId)}/unassign`, { body, auth: true });
  }
  /** Set the menu tree for a client. */
  async permissionSetMenus(clientId, body) {
    return this.request("PUT", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/menus`, { body, auth: true });
  }
  /** List roles for a client. */
  async permissionListRoles(clientId) {
    return this.request("GET", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/roles`, { auth: true });
  }
  /** Add a role to the client's role registry. */
  async permissionAddRole(clientId, body) {
    return this.request("POST", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/roles`, { body, auth: true });
  }
  /** Remove a role. */
  async permissionRemoveRole(clientId, roleCode) {
    return this.request("DELETE", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/roles/${encodeURIComponent(roleCode)}`, { auth: true });
  }
  /** Update a role. */
  async permissionUpdateRole(clientId, roleCode, body) {
    return this.request("PUT", `/api/v1/admin/permissions/${encodeURIComponent(clientId)}/roles/${encodeURIComponent(roleCode)}`, { body, auth: true });
  }
  /** List registered authentication providers. */
  async listAdminProviders() {
    return this.request("GET", `/api/v1/admin/providers`, { auth: true });
  }
  /** Register an authentication provider. */
  async createAdminProvider() {
    return this.request("POST", `/api/v1/admin/providers`, { auth: true });
  }
  /** Delete one authentication provider. */
  async deleteAdminProvider(id) {
    return this.request("DELETE", `/api/v1/admin/providers/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Get one authentication provider. */
  async getAdminProvider(id) {
    return this.request("GET", `/api/v1/admin/providers/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Replace one authentication provider. */
  async updateAdminProvider(id) {
    return this.request("PUT", `/api/v1/admin/providers/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Evaluate a ReBAC relationship-tuple Check query (operational debugging). */
  async rebacCheck(query) {
    return this.request("GET", `/api/v1/admin/rebac/check`, { query, auth: true });
  }
  /** List registered releases. */
  async releaseList() {
    return this.request("GET", `/api/v1/admin/releases`, { auth: true });
  }
  /** Register a paired frontend+backend release. */
  async releaseRegister(body) {
    return this.request("POST", `/api/v1/admin/releases`, { body, auth: true });
  }
  /** Delete a registered release. */
  async releaseDelete(id) {
    return this.request("DELETE", `/api/v1/admin/releases/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a single release by id. */
  async releaseGet(id) {
    return this.request("GET", `/api/v1/admin/releases/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Pin (make current) a registered release. */
  async releasePin(id) {
    return this.request("POST", `/api/v1/admin/releases/${encodeURIComponent(id)}:pin`, { auth: true });
  }
  /** Rollback to a previously-pinned release. */
  async releaseRollback(id) {
    return this.request("POST", `/api/v1/admin/releases/${encodeURIComponent(id)}:rollback`, { auth: true });
  }
  /** Fetch the currently-pinned release. */
  async releaseGetCurrent() {
    return this.request("GET", `/api/v1/admin/releases:current`, { auth: true });
  }
  /** List security activity across users and devices. */
  async listAdminSecurityActivity() {
    return this.request("GET", `/api/v1/admin/security/activity`, { auth: true });
  }
  /** List every active session. */
  async getAdminSessions() {
    return this.request("GET", `/api/v1/admin/sessions`, { auth: true });
  }
  /** Cross-protocol session-hub query (every session, every protocol, for one subject). */
  async getAdminLinkedSessions(subject) {
    return this.request("GET", `/api/v1/admin/sessions/linked/${encodeURIComponent(subject)}`, { auth: true });
  }
  /** List stored snapshots. */
  async snapshotList() {
    return this.request("GET", `/api/v1/admin/snapshots`, { auth: true });
  }
  /** Export a snapshot of operator-managed state. */
  async snapshotExport(body) {
    return this.request("POST", `/api/v1/admin/snapshots`, { body, auth: true });
  }
  /** Delete a stored snapshot. */
  async snapshotDelete(id) {
    return this.request("DELETE", `/api/v1/admin/snapshots/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a single snapshot (header + server-redacted resources). */
  async snapshotGet(id) {
    return this.request("GET", `/api/v1/admin/snapshots/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Restore a snapshot. */
  async snapshotRestore(id, body) {
    return this.request("POST", `/api/v1/admin/snapshots/${encodeURIComponent(id)}:restore`, { body, auth: true });
  }
  /** Per-store storage-health report (reachability + schema version + latency). */
  async getStorageHealth() {
    return this.request("GET", `/api/v1/admin/storage-health`, { auth: true });
  }
  /** List tenants. */
  async tenantList() {
    return this.request("GET", `/api/v1/admin/tenants`, { auth: true });
  }
  /** Create a tenant. */
  async tenantCreate(body) {
    return this.request("POST", `/api/v1/admin/tenants`, { body, auth: true });
  }
  /** Delete a tenant. */
  async tenantDelete(id) {
    return this.request("DELETE", `/api/v1/admin/tenants/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a tenant. */
  async tenantGet(id) {
    return this.request("GET", `/api/v1/admin/tenants/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Update a tenant (preserves status). */
  async tenantUpdate(id, body) {
    return this.request("PUT", `/api/v1/admin/tenants/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Export a whole tenant's data (offboarding / environment migration). */
  async adminExportTenant(id) {
    return this.request("POST", `/api/v1/admin/tenants/${encodeURIComponent(id)}/export`, { auth: true });
  }
  /** List a tenant's pending org invitations (no token value). */
  async adminListInvitations(id) {
    return this.request("GET", `/api/v1/admin/tenants/${encodeURIComponent(id)}/invitations`, { auth: true });
  }
  /** Send an org invitation. */
  async adminSendInvitation(id, body) {
    return this.request("POST", `/api/v1/admin/tenants/${encodeURIComponent(id)}/invitations`, { body, auth: true });
  }
  /** Revoke every pending invitation for a recipient email. */
  async adminRevokeInvitation(id, email) {
    return this.request("DELETE", `/api/v1/admin/tenants/${encodeURIComponent(id)}/invitations/${encodeURIComponent(email)}`, { auth: true });
  }
  /** List a tenant's org roster (B2B membership). */
  async adminListTenantMembers(id) {
    return this.request("GET", `/api/v1/admin/tenants/${encodeURIComponent(id)}/members`, { auth: true });
  }
  /** Remove a user from an org. */
  async adminRemoveTenantMember(id, userId) {
    return this.request("DELETE", `/api/v1/admin/tenants/${encodeURIComponent(id)}/members/${encodeURIComponent(userId)}`, { auth: true });
  }
  /** Add a user to an org or change their org role. */
  async adminPutTenantMember(id, userId, body) {
    return this.request("PUT", `/api/v1/admin/tenants/${encodeURIComponent(id)}/members/${encodeURIComponent(userId)}`, { body, auth: true });
  }
  /** Per-tenant usage/metering report. */
  async getTenantUsage(id, query) {
    return this.request("GET", `/api/v1/admin/tenants/${encodeURIComponent(id)}/usage`, { query, auth: true });
  }
  /** Flip a tenant's suspension status. */
  async tenantSetStatus(id, body) {
    return this.request("POST", `/api/v1/admin/tenants/${encodeURIComponent(id)}:set-status`, { body, auth: true });
  }
  /** Active ITDR threat-policy list. */
  async getAdminThreatPolicies() {
    return this.request("GET", `/api/v1/admin/threat-policies`, { auth: true });
  }
  /** Delete a threat policy by name. */
  async deleteAdminThreatPolicy(name) {
    return this.request("DELETE", `/api/v1/admin/threat-policies/${encodeURIComponent(name)}`, { auth: true });
  }
  /** Get a threat policy by name. */
  async getAdminThreatPolicy(name) {
    return this.request("GET", `/api/v1/admin/threat-policies/${encodeURIComponent(name)}`, { auth: true });
  }
  /** Create or update a threat policy. */
  async putAdminThreatPolicy(name, body) {
    return this.request("PUT", `/api/v1/admin/threat-policies/${encodeURIComponent(name)}`, { body, auth: true });
  }
  /** Active token-policy governance view. */
  async getAdminTokenPolicies() {
    return this.request("GET", `/api/v1/admin/token-policies`, { auth: true });
  }
  /** RFC 8693 token-exchange delegation-chain lookup. */
  async getAdminTokenExchangeChain(jti) {
    return this.request("GET", `/api/v1/admin/tokenexchange/chains/${encodeURIComponent(jti)}`, { auth: true });
  }
  /** List active admin bearer tokens. */
  async getAdminTokens(query) {
    return this.request("GET", `/api/v1/admin/tokens`, { query, auth: true });
  }
  /** Bulk-revoke workflow. */
  async postAdminTokenBulkRevoke(body) {
    return this.request("POST", `/api/v1/admin/tokens/bulk-revoke`, { body, auth: true });
  }
  /** Refresh-token expiry calendar. */
  async getAdminTokenExpiring(query) {
    return this.request("GET", `/api/v1/admin/tokens/expiring`, { query, auth: true });
  }
  /** Token portfolio overview. */
  async getAdminTokenPortfolio(query) {
    return this.request("GET", `/api/v1/admin/tokens/portfolio`, { query, auth: true });
  }
  /** Revoke a token or session. */
  async adminTokenRevoke(body) {
    return this.request("POST", `/api/v1/admin/tokens/revoke`, { body, auth: true });
  }
  /** List active session-backed tokens. */
  async adminTokenListSessions(query) {
    return this.request("GET", `/api/v1/admin/tokens/sessions`, { query, auth: true });
  }
  /** Per-subject active-token view. */
  async getAdminTokenSubject(subject, query) {
    return this.request("GET", `/api/v1/admin/tokens/subjects/${encodeURIComponent(subject)}`, { query, auth: true });
  }
  /** Suspicious-token anomaly list. */
  async getAdminTokenSuspicious(query) {
    return this.request("GET", `/api/v1/admin/tokens/suspicious`, { query, auth: true });
  }
  /** Issue a temp token for a user. */
  async adminTokenIssueTemp(body) {
    return this.request("POST", `/api/v1/admin/tokens/temp`, { body, auth: true });
  }
  /** Aggregated token-usage telemetry. */
  async getAdminTokenUsage(query) {
    return this.request("GET", `/api/v1/admin/tokens/usage`, { query, auth: true });
  }
  /** Revoke a single admin bearer token by ID. */
  async deleteAdminToken(id) {
    return this.request("DELETE", `/api/v1/admin/tokens/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Top-tenants usage leaderboard. */
  async getAdminTopTenants(query) {
    return this.request("GET", `/api/v1/admin/usage/top-tenants`, { query, auth: true });
  }
  /** List users. */
  async adminUserList(query) {
    return this.request("GET", `/api/v1/admin/users`, { query, auth: true });
  }
  /** Create a user. */
  async adminUserCreate(body) {
    return this.request("POST", `/api/v1/admin/users`, { body, auth: true });
  }
  /** Delete a user. */
  async adminUserDelete(id) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch a user. */
  async adminUserGet(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Update a user. */
  async adminUserUpdate(id, body) {
    return this.request("PUT", `/api/v1/admin/users/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** List a user's consent grants (helpdesk). */
  async adminUserListConsents(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/consents`, { auth: true });
  }
  /** Revoke a user's consent for an app (helpdesk). */
  async adminUserRevokeConsent(id, clientId) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/consents/${encodeURIComponent(clientId)}`, { auth: true });
  }
  /** Revoke a user's Native SSO device secrets (lost-device lockout). */
  async adminUserRevokeDeviceSecrets(id) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/device-secrets`, { auth: true });
  }
  /** List devices for one user. */
  async listAdminUserDevices(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/devices`, { auth: true });
  }
  /** Revoke one device owned by a user. */
  async deleteAdminUserDevice(id, deviceId) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/devices/${encodeURIComponent(deviceId)}`, { auth: true });
  }
  /** Force-set a user's email (operational recovery). */
  async adminUserSetEmail(id, body) {
    return this.request("POST", `/api/v1/admin/users/${encodeURIComponent(id)}/email`, { body, auth: true });
  }
  /** Revoke a user's pending email-change verification tokens. */
  async adminUserRevokeEmailChangeTokens(id) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/email-change-tokens`, { auth: true });
  }
  /** List a user's pending email-change tokens (no token value). */
  async adminUserListEmailChangeTokens(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/email-change-tokens`, { auth: true });
  }
  /** Get a user's lifecycle state, legal transitions, and history. */
  async adminUserGetLifecycle(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/lifecycle`, { auth: true });
  }
  /** Request a user-lifecycle state transition. */
  async adminUserTransitionLifecycle(id, body) {
    return this.request("POST", `/api/v1/admin/users/${encodeURIComponent(id)}/lifecycle`, { body, auth: true });
  }
  /** List login history for one user. */
  async getAdminUserLoginHistory(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/login-history`, { auth: true });
  }
  /** List a user's registered second factors (helpdesk). */
  async adminUserListMFA(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/mfa`, { auth: true });
  }
  /** Reset a user's MFA recovery codes (helpdesk). */
  async adminUserResetRecoveryCodes(id) {
    return this.request("POST", `/api/v1/admin/users/${encodeURIComponent(id)}/mfa/recovery-codes`, { auth: true });
  }
  /** Unbind a user's second factor (helpdesk MFA reset). */
  async adminUserRemoveMFA(id, factorId) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/mfa/${encodeURIComponent(factorId)}`, { auth: true });
  }
  /** Set a user's password (helpdesk reset). */
  async adminUserResetPassword(id, body) {
    return this.request("POST", `/api/v1/admin/users/${encodeURIComponent(id)}/password`, { body, auth: true });
  }
  /** Revoke a user's pending forgot-password tokens. */
  async adminUserRevokePasswordResetTokens(id) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/password-reset-tokens`, { auth: true });
  }
  /** List a user's pending forgot-password tokens (no token value). */
  async adminUserListPasswordResetTokens(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/password-reset-tokens`, { auth: true });
  }
  /** Revoke every OAuth 2.0 refresh token a user holds, across all clients. */
  async adminUserRevokeRefreshTokens(id) {
    return this.request("DELETE", `/api/v1/admin/users/${encodeURIComponent(id)}/refresh-tokens`, { auth: true });
  }
  /** List a user's active sessions. */
  async adminUserListSessions(id) {
    return this.request("GET", `/api/v1/admin/users/${encodeURIComponent(id)}/sessions`, { auth: true });
  }
  /** Evaluate the hosted WASM authorization policy (operational debugging). */
  async wasmAuthzCheck(body) {
    return this.request("POST", `/api/v1/admin/wasmauthz/check`, { body, auth: true });
  }
  /** List dead-lettered webhook deliveries. */
  async webhookListDeadLetters(query) {
    return this.request("GET", `/api/v1/admin/webhooks/deadletters`, { query, auth: true });
  }
  /** Replay a dead-lettered webhook delivery. */
  async webhookReplayDeadLetter(id) {
    return this.request("POST", `/api/v1/admin/webhooks/deadletters/${encodeURIComponent(id)}/replay`, { auth: true });
  }
  /** List generic event/webhook egress subscriptions. */
  async webhookListSubscriptions() {
    return this.request("GET", `/api/v1/admin/webhooks/subscriptions`, { auth: true });
  }
  /** Register a webhook egress subscription. */
  async webhookCreateSubscription(body) {
    return this.request("POST", `/api/v1/admin/webhooks/subscriptions`, { body, auth: true });
  }
  /** Delete a webhook egress subscription. */
  async webhookDeleteSubscription(id) {
    return this.request("DELETE", `/api/v1/admin/webhooks/subscriptions/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Query audit events. */
  async queryAuditEvents(query) {
    return this.request("GET", `/api/v1/audit/events`, { query, auth: true });
  }
  /** Fetch one audit event by id. */
  async getAuditEvent(id) {
    return this.request("GET", `/api/v1/audit/events/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Aggregate audit-event facet counts. */
  async queryAuditFacets(query) {
    return this.request("GET", `/api/v1/audit/facets`, { query, auth: true });
  }
  /** Fetch one Client record. */
  async getClientByID(id) {
    return this.request("GET", `/api/v1/clients/${encodeURIComponent(id)}`, {});
  }
  /** Erase a subject's data (GDPR Art. 17). */
  async eraseSubject(id, body) {
    return this.request("POST", `/api/v1/compliance/users/${encodeURIComponent(id)}/erase`, { body, auth: true });
  }
  /** Export a subject's data (GDPR Art. 15 / 20). */
  async exportSubject(id) {
    return this.request("GET", `/api/v1/compliance/users/${encodeURIComponent(id)}/export`, { auth: true });
  }
  /** Classify an arbitrary (remote_addr, host) pair. */
  async classifyNetPolicy(query) {
    return this.request("GET", `/api/v1/netpolicy/classify`, { query, auth: true });
  }
  /** List network policies. */
  async listNetPolicies() {
    return this.request("GET", `/api/v1/netpolicy/policies`, { auth: true });
  }
  /** Apply (create or overwrite) a network policy. */
  async applyNetPolicy(body) {
    return this.request("POST", `/api/v1/netpolicy/policies`, { body, auth: true });
  }
  /** Delete a network policy. */
  async deleteNetPolicy(name) {
    return this.request("DELETE", `/api/v1/netpolicy/policies/${encodeURIComponent(name)}`, { auth: true });
  }
  /** Fetch a single network policy. */
  async getNetPolicy(name) {
    return this.request("GET", `/api/v1/netpolicy/policies/${encodeURIComponent(name)}`, { auth: true });
  }
  // ---- auth ----
  /** Upstream IdP federation return URL. */
  async getAuthCallback(query) {
    return this.request("GET", `/auth/callback`, { query });
  }
  /** Begin an unauthenticated password reset. */
  async forgotPassword(body) {
    return this.request("POST", `/auth/forgot-password`, { body });
  }
  /** B2B home-realm discovery — resolve an email domain to its IdP. */
  async homeRealmDiscovery(body) {
    return this.request("POST", `/auth/home-realm`, { body });
  }
  /** Start a browser-based federated login. */
  async getLogin(query) {
    return this.request("GET", `/auth/login`, { query });
  }
  /** Authenticate and receive a token. */
  async postLogin(body) {
    return this.request("POST", `/auth/login`, { body });
  }
  /** Complete an MFA step-up challenge. */
  async postMFAComplete(body) {
    return this.request("POST", `/auth/mfa`, { body });
  }
  /** Self-service signup (opt-in, default-off). */
  async selfRegister(body) {
    return this.request("POST", `/auth/register`, { body });
  }
  /** Complete a password reset with a token. */
  async resetPassword(body) {
    return this.request("POST", `/auth/reset-password`, { body });
  }
  /** Send a one-time code for the named authenticator. */
  async postSendCode(body) {
    return this.request("POST", `/auth/send-code`, { body });
  }
  /** Consume a self-service email verification token. */
  async verifyEmail(body) {
    return this.request("POST", `/auth/verify-email`, { body });
  }
  /** Device-flow initiation (RFC 8628 §3.1). */
  async postDeviceCode(body) {
    return this.request("POST", `/device/code`, { body });
  }
  /** Render the device authorization verification page. */
  async getDeviceVerify() {
    return this.request("GET", `/device/verify`, {});
  }
  /** User-side device-code approval (RFC 8628 §3.3). */
  async postDeviceVerify(body) {
    return this.request("POST", `/device/verify`, { body, auth: true });
  }
  /** OpenID Connect RP-Initiated Logout 1.0. */
  async getEndSession(query) {
    return this.request("GET", `/end_session`, { query });
  }
  /** Return client-specific login UI metadata. */
  async getLoginUIMetadata(query) {
    return this.request("GET", `/login-ui/metadata`, { query });
  }
  /** Revoke the current bearer token and / or named session. */
  async postLogout(body) {
    return this.request("POST", `/logout`, { body, auth: true });
  }
  /** Pushed Authorization Request (RFC 9126). */
  async postPAR(body) {
    return this.request("POST", `/par`, { body });
  }
  /** Dynamic Client Registration (RFC 7591). */
  async postRegister(body) {
    return this.request("POST", `/register`, { body, auth: true });
  }
  /** Deregister the client (RFC 7592 §2.3). */
  async deleteRegistration(clientId) {
    return this.request("DELETE", `/register/${encodeURIComponent(clientId)}`, { auth: true });
  }
  /** Read current DCR metadata (RFC 7592 §2.1). */
  async getRegistration(clientId) {
    return this.request("GET", `/register/${encodeURIComponent(clientId)}`, { auth: true });
  }
  /** Update DCR metadata (RFC 7592 §2.2). */
  async putRegistration(clientId, body) {
    return this.request("PUT", `/register/${encodeURIComponent(clientId)}`, { body, auth: true });
  }
  /** OAuth 2.0 token endpoint (RFC 6749 §3.2). */
  async postToken(body) {
    return this.request("POST", `/token`, { body });
  }
  /** OAuth 2.0 token introspection (RFC 7662). */
  async postIntrospect(body) {
    return this.request("POST", `/token/introspect`, { body });
  }
  /** OAuth 2.0 token revocation (RFC 7009). */
  async postRevoke(body) {
    return this.request("POST", `/token/revoke`, { body });
  }
  /** Bulk revoke every refresh token bound to the bearer's subject. */
  async postRevokeAll() {
    return this.request("POST", `/token/revoke-all`, { auth: true });
  }
  // ---- authorization ----
  /** Check whether a subject has a relation to an object. */
  async checkAuthorization() {
    return this.request("GET", `/authz/check`, { auth: true });
  }
  /** Reverse-expand an authorization relation graph. */
  async expandAuthorizationGraph() {
    return this.request("GET", `/authz/graph`, { auth: true });
  }
  /** Delete an authorization tuple. */
  async deleteAuthorizationTuple() {
    return this.request("DELETE", `/authz/tuples`, { auth: true });
  }
  /** List authorization tuples. */
  async listAuthorizationTuples() {
    return this.request("GET", `/authz/tuples`, { auth: true });
  }
  /** Write an authorization tuple. */
  async writeAuthorizationTuple() {
    return this.request("POST", `/authz/tuples`, { auth: true });
  }
  /** Atomically apply a batch of authorization tuple mutations. */
  async batchWriteAuthorizationTuples(body) {
    return this.request("POST", `/authz/tuples/batch`, { body, auth: true });
  }
  // ---- discovery ----
  /** JSON Web Key Set for local JWT verification. */
  async getJWKS() {
    return this.request("GET", `/.well-known/jwks.json`, {});
  }
  /** RFC 8414 OAuth 2.0 Authorization Server Metadata (alias). */
  async getOAuthAuthorizationServerMetadata() {
    return this.request("GET", `/.well-known/oauth-authorization-server`, {});
  }
  /** OAuth 2.0 Protected Resource Metadata (RFC 9728). */
  async protectedResourceMetadata() {
    return this.request("GET", `/.well-known/oauth-protected-resource`, {});
  }
  /** OpenID Connect Discovery 1.0 document. */
  async getOpenIDConfiguration() {
    return this.request("GET", `/.well-known/openid-configuration`, {});
  }
  /** Classify the caller's own (remote_addr, host). */
  async resolveMeNetPolicy() {
    return this.request("GET", `/api/v1/netpolicy/resolve-me`, {});
  }
  /** First-run provisioning (opt-in, public, single-use). */
  async postSetup(body) {
    return this.request("POST", `/api/v1/setup`, { body });
  }
  /** First-run setup status (opt-in, public). */
  async getSetupStatus() {
    return this.request("GET", `/api/v1/setup/status`, {});
  }
  /** ADR-0008 v2alpha proof-of-mechanism route (opt-in, preview). */
  async getAPIVersionPreview() {
    return this.request("GET", `/api/v2alpha/version`, {});
  }
  /** OpenID Connect Session Management 1.0 OP iframe. */
  async getCheckSessionIframe() {
    return this.request("GET", `/check_session_iframe`, {});
  }
  /** Liveness + identity probe. */
  async getHealth() {
    return this.request("GET", `/health`, {});
  }
  // ---- federation ----
  /** OpenID Federation 1.0 entity configuration. */
  async getFederationEntityConfiguration() {
    return this.request("GET", `/.well-known/openid-federation`, {});
  }
  /** Return historical federation verification keys. */
  async getFederationHistoricalKeys() {
    return this.request("GET", `/.well-known/openid-federation-historical-keys`, {});
  }
  /** List configured federation subordinates. */
  async listFederationSubordinates() {
    return this.request("GET", `/.well-known/openid-federation-list`, {});
  }
  /** OpenID Federation 1.0 §8.3 trust-chain resolution. */
  async resolveFederationTrustChain(query) {
    return this.request("GET", `/.well-known/openid-federation-resolve`, { query });
  }
  /** Resolve the status of a federation trust mark. */
  async getFederationTrustMarkStatus(query) {
    return this.request("GET", `/.well-known/openid-federation-trust-mark-status`, { query });
  }
  /** Discover the home realm for a browser login identifier. */
  async getHomeRealm(query) {
    return this.request("GET", `/auth/home-realm`, { query });
  }
  /** OpenID Federation 1.0 §8 Federation Fetch endpoint. */
  async getFederationFetch(query) {
    return this.request("GET", `/fetch`, { query });
  }
  // ---- me ----
  /** List physical devices owned by the authenticated subject. */
  async listMyPhysicalDevices() {
    return this.request("GET", `/me/devices`, { auth: true });
  }
  /** Delete one physical device and revoke its associated sessions. */
  async deleteMyPhysicalDevice(id) {
    return this.request("DELETE", `/me/devices/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Get one device owned by the authenticated subject. */
  async getMyDevice(id) {
    return this.request("GET", `/me/devices/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Update the display metadata for one owned device. */
  async patchMyDevice(id, body) {
    return this.request("PATCH", `/me/devices/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Menu tree the bearer's subject is authorized to see. */
  async getMyMenus() {
    return this.request("GET", `/menus/me`, { auth: true });
  }
  /** Permissions of the bearer's subject for the inferred client. */
  async getMyPermissions() {
    return this.request("GET", `/permissions/me`, { auth: true });
  }
  /** Roles of the bearer's subject for the inferred client. */
  async getMyRoles() {
    return this.request("GET", `/roles/me`, { auth: true });
  }
  // ---- mesh ----
  /** Envoy/Istio ext_authz HTTP-mode authorization check (opt-in). */
  async meshExtAuthz() {
    return this.request("GET", `/mesh/ext-authz`, { auth: true });
  }
  // ---- operational ----
  /** Return the versioned API status document. */
  async getRuntimeStatus() {
    return this.request("GET", `/api/v1/status`, {});
  }
  /** Liveness probe. */
  async getLivez() {
    return this.request("GET", `/livez`, {});
  }
  /** Readiness probe — aggregates every registered ReadyCheck. */
  async getReadyz() {
    return this.request("GET", `/readyz`, {});
  }
  // ---- scim ----
  /** SCIM bulk operations (RFC 7644 §3.7). */
  async scimBulk(body) {
    return this.request("POST", `/api/v1/scim/v2/Bulk`, { body, auth: true });
  }
  /** List / search SCIM Groups (RFC 7644 §3.4). */
  async scimListGroups(query) {
    return this.request("GET", `/api/v1/scim/v2/Groups`, { query, auth: true });
  }
  /** Create a SCIM Group (RFC 7643 §4.2). */
  async scimCreateGroup(body) {
    return this.request("POST", `/api/v1/scim/v2/Groups`, { body, auth: true });
  }
  /** Delete a SCIM Group (RFC 7644 §3.6). */
  async scimDeleteGroup(id) {
    return this.request("DELETE", `/api/v1/scim/v2/Groups/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch one SCIM Group. */
  async scimGetGroup(id) {
    return this.request("GET", `/api/v1/scim/v2/Groups/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Patch a SCIM Group (RFC 7644 §3.5.2). */
  async scimPatchGroup(id, body) {
    return this.request("PATCH", `/api/v1/scim/v2/Groups/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Replace a SCIM Group (RFC 7644 §3.5.1). */
  async scimReplaceGroup(id, body) {
    return this.request("PUT", `/api/v1/scim/v2/Groups/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Delete the authenticated subject's own SCIM User (RFC 7644 §3.11). */
  async scimMeDelete() {
    return this.request("DELETE", `/api/v1/scim/v2/Me`, { auth: true });
  }
  /** The authenticated subject's own SCIM User resource (RFC 7644 §3.11). */
  async scimMeGet() {
    return this.request("GET", `/api/v1/scim/v2/Me`, { auth: true });
  }
  /** Modify the authenticated subject's own SCIM User (RFC 7644 §3.11). */
  async scimMePatch(body) {
    return this.request("PATCH", `/api/v1/scim/v2/Me`, { body, auth: true });
  }
  /** Replace the authenticated subject's own SCIM User (RFC 7644 §3.11). */
  async scimMePut(body) {
    return this.request("PUT", `/api/v1/scim/v2/Me`, { body, auth: true });
  }
  /** SCIM resource schemas (RFC 7643 §7). */
  async scimSchemas() {
    return this.request("GET", `/api/v1/scim/v2/Schemas`, { auth: true });
  }
  /** SCIM service-provider configuration (RFC 7643 §5). */
  async scimServiceProviderConfig() {
    return this.request("GET", `/api/v1/scim/v2/ServiceProviderConfig`, { auth: true });
  }
  /** List / search SCIM Users (RFC 7644 §3.4). */
  async scimListUsers(query) {
    return this.request("GET", `/api/v1/scim/v2/Users`, { query, auth: true });
  }
  /** Create a SCIM User (RFC 7644 §3.3). */
  async scimCreateUser(body) {
    return this.request("POST", `/api/v1/scim/v2/Users`, { body, auth: true });
  }
  /** Delete a SCIM User (RFC 7644 §3.6). */
  async scimDeleteUser(id) {
    return this.request("DELETE", `/api/v1/scim/v2/Users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Fetch one SCIM User (RFC 7644 §3.4.1). */
  async scimGetUser(id) {
    return this.request("GET", `/api/v1/scim/v2/Users/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Patch a SCIM User (RFC 7644 §3.5.2). */
  async scimPatchUser(id, body) {
    return this.request("PATCH", `/api/v1/scim/v2/Users/${encodeURIComponent(id)}`, { body, auth: true });
  }
  /** Replace a SCIM User (RFC 7644 §3.5.1). */
  async scimReplaceUser(id, body) {
    return this.request("PUT", `/api/v1/scim/v2/Users/${encodeURIComponent(id)}`, { body, auth: true });
  }
  // ---- self-service ----
  /** Public per-host white-label branding for the hosted login SPA. */
  async getBranding() {
    return this.request("GET", `/branding`, {});
  }
  /** List the authenticated user's consent grants. */
  async listMyConsents() {
    return this.request("GET", `/consents/me`, { auth: true });
  }
  /** Revoke the authenticated user's consent for a client. */
  async deleteMyConsent(clientId) {
    return this.request("DELETE", `/consents/me/${encodeURIComponent(clientId)}`, { auth: true });
  }
  /** Authenticated self-service account overview. */
  async getMe() {
    return this.request("GET", `/me`, { auth: true });
  }
  /** Authenticated self-service profile update. */
  async patchMe(body) {
    return this.request("PATCH", `/me`, { body, auth: true });
  }
  /** Erase the authenticated user's own account (GDPR Art. 17). */
  async eraseMyAccount(body) {
    return this.request("POST", `/me/account/erase`, { body, auth: true });
  }
  /** Export the authenticated user's own data (GDPR Art. 15). */
  async exportMyData() {
    return this.request("GET", `/me/data-export`, { auth: true });
  }
  /** Get activity for one owned device. */
  async getMyDeviceActivity(id) {
    return this.request("GET", `/me/devices/${encodeURIComponent(id)}/activity`, { auth: true });
  }
  /** Report an owned device lost and revoke its sessions. */
  async reportMyDeviceLost(id) {
    return this.request("POST", `/me/devices/${encodeURIComponent(id)}/lost`, { auth: true });
  }
  /** List sessions associated with one owned device. */
  async getMyDeviceSessions(id) {
    return this.request("GET", `/me/devices/${encodeURIComponent(id)}/sessions`, { auth: true });
  }
  /** Change the trust state of one owned device. */
  async setMyDeviceTrust(id) {
    return this.request("POST", `/me/devices/${encodeURIComponent(id)}/trust`, { auth: true });
  }
  /** Begin a verified email change. */
  async changeMyEmail(body) {
    return this.request("POST", `/me/email/change`, { body, auth: true });
  }
  /** Complete a verified email change. */
  async verifyMyEmail(body) {
    return this.request("POST", `/me/email/verify`, { body, auth: true });
  }
  /** List the authenticated user's linked external identities. */
  async listMyIdentities() {
    return this.request("GET", `/me/identities`, { auth: true });
  }
  /** Unlink one of the authenticated user's own linked identities. */
  async deleteMyIdentity(id) {
    return this.request("DELETE", `/me/identities/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Accept an org invitation (join the org). */
  async acceptInvitation(body) {
    return this.request("POST", `/me/invitations/accept`, { body, auth: true });
  }
  /** List login history for the authenticated user. */
  async getMyLoginHistory() {
    return this.request("GET", `/me/login-history`, { auth: true });
  }
  /** List the authenticated user's registered second factors. */
  async listMyMFAFactors() {
    return this.request("GET", `/me/mfa`, { auth: true });
  }
  /** Count the caller's remaining recovery codes. */
  async countMyRecoveryCodes() {
    return this.request("GET", `/me/mfa/recovery-codes`, { auth: true });
  }
  /** Regenerate the caller's single-use MFA recovery codes. */
  async regenerateMyRecoveryCodes() {
    return this.request("POST", `/me/mfa/recovery-codes`, { auth: true });
  }
  /** Begin self-service TOTP enrollment. */
  async beginMyTOTPEnrollment() {
    return this.request("POST", `/me/mfa/totp/begin`, { auth: true });
  }
  /** Confirm and commit a self-service TOTP factor. */
  async confirmMyTOTPEnrollment(body) {
    return this.request("POST", `/me/mfa/totp/confirm`, { body, auth: true });
  }
  /** Begin authenticated self-service passkey registration. */
  async beginMyPasskeyRegistration(body) {
    return this.request("POST", `/me/mfa/webauthn/begin`, { body, auth: true });
  }
  /** Finish authenticated self-service passkey registration. */
  async finishMyPasskeyRegistration(query, body) {
    return this.request("POST", `/me/mfa/webauthn/finish`, { query, body, auth: true });
  }
  /** Unbind one of the authenticated user's second factors. */
  async deleteMyMFAFactor(id) {
    return this.request("DELETE", `/me/mfa/${encodeURIComponent(id)}`, { auth: true });
  }
  /** List the orgs the authenticated user belongs to. */
  async listMyOrganizations() {
    return this.request("GET", `/me/organizations`, { auth: true });
  }
  /** Leave an organization (self-service). */
  async leaveMyOrganization(tenantId) {
    return this.request("DELETE", `/me/organizations/${encodeURIComponent(tenantId)}`, { auth: true });
  }
  /** List pending invitations for an org you administer (no token value). */
  async orgAdminListInvitations(tenantId) {
    return this.request("GET", `/me/organizations/${encodeURIComponent(tenantId)}/invitations`, { auth: true });
  }
  /** Send an invitation for an org you administer (delegated org-admin). */
  async orgAdminSendInvitation(tenantId, body) {
    return this.request("POST", `/me/organizations/${encodeURIComponent(tenantId)}/invitations`, { body, auth: true });
  }
  /** Revoke every pending invitation for a recipient (delegated org-admin). */
  async orgAdminRevokeInvitation(tenantId, email) {
    return this.request("DELETE", `/me/organizations/${encodeURIComponent(tenantId)}/invitations/${encodeURIComponent(email)}`, { auth: true });
  }
  /** List the roster of an org you administer (delegated org-admin). */
  async orgAdminListMembers(tenantId) {
    return this.request("GET", `/me/organizations/${encodeURIComponent(tenantId)}/members`, { auth: true });
  }
  /** Remove a member from an org you administer (delegated org-admin). */
  async orgAdminRemoveMember(tenantId, userId) {
    return this.request("DELETE", `/me/organizations/${encodeURIComponent(tenantId)}/members/${encodeURIComponent(userId)}`, { auth: true });
  }
  /** Change an existing member's org role (delegated org-admin). */
  async orgAdminPutMember(tenantId, userId, body) {
    return this.request("PUT", `/me/organizations/${encodeURIComponent(tenantId)}/members/${encodeURIComponent(userId)}`, { body, auth: true });
  }
  /** Authenticated self-service password change. */
  async changeMyPassword(body) {
    return this.request("POST", `/me/password`, { body, auth: true });
  }
  /** List the authenticated user's security activity. */
  async getMySecurityActivity() {
    return this.request("GET", `/me/security/activity`, { auth: true });
  }
  /** List the authenticated user's active sessions. */
  async getMeSessions() {
    return this.request("GET", `/me/sessions`, { auth: true });
  }
  /** List active sessions enriched with device metadata. */
  async getEnrichedMeSessions() {
    return this.request("GET", `/me/sessions/enriched`, { auth: true });
  }
  /** Revoke all sessions owned by the authenticated user. */
  async revokeAllMeSessions() {
    return this.request("POST", `/me/sessions/revoke-all`, { auth: true });
  }
  /** Revoke one session owned by the authenticated user. */
  async deleteMeSession(id) {
    return this.request("DELETE", `/me/sessions/${encodeURIComponent(id)}`, { auth: true });
  }
  /** List the authenticated user's trusted (MFA-skip) devices. */
  async listMyTrustedDevices() {
    return this.request("GET", `/me/trusted-devices`, { auth: true });
  }
  /** Mark the current device trusted, skipping MFA on future logins. */
  async trustMyDevice(body) {
    return this.request("POST", `/me/trusted-devices/trust`, { body, auth: true });
  }
  /** Revoke one of the authenticated user's trusted-device grants. */
  async revokeMyTrustedDevice(id) {
    return this.request("DELETE", `/me/trusted-devices/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Sign out everywhere — revoke the user's sessions in bulk. */
  async revokeMySessions(query) {
    return this.request("DELETE", `/sessions/me`, { query, auth: true });
  }
  /** List the authenticated user's own active sessions. */
  async listMySessions() {
    return this.request("GET", `/sessions/me`, { auth: true });
  }
  /** Revoke one of the authenticated user's own sessions. */
  async deleteMySession(id) {
    return this.request("DELETE", `/sessions/me/${encodeURIComponent(id)}`, { auth: true });
  }
  // ---- ssf ----
  /** Return Shared Signals Framework transmitter metadata. */
  async getSSFConfiguration() {
    return this.request("GET", `/.well-known/ssf-configuration`, {});
  }
  /** OpenID Shared Signals (CAEP/SSF) push-delivery receiver (opt-in). */
  async ssfReceive(body) {
    return this.request("POST", `/ssf/receive`, { body });
  }
  /** List Shared Signals delivery streams. */
  async listSSFStreams() {
    return this.request("GET", `/ssf/streams`, { auth: true });
  }
  /** Create a Shared Signals delivery stream. */
  async createSSFStream(body) {
    return this.request("POST", `/ssf/streams`, { body, auth: true });
  }
  /** Delete one Shared Signals delivery stream. */
  async deleteSSFStream(id) {
    return this.request("DELETE", `/ssf/streams/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Get one Shared Signals delivery stream. */
  async getSSFStream(id) {
    return this.request("GET", `/ssf/streams/${encodeURIComponent(id)}`, { auth: true });
  }
  /** Replace one Shared Signals delivery stream. */
  async updateSSFStream(id, body) {
    return this.request("PUT", `/ssf/streams/${encodeURIComponent(id)}`, { body, auth: true });
  }
  // ---- userinfo ----
  /** Fetch the user record for the bearer's subject. */
  async getUserInfo() {
    return this.request("GET", `/userinfo`, { auth: true });
  }
  // ---- webauthn ----
  /** Start a WebAuthn login (assertion) ceremony. */
  async postWebAuthnLoginBegin(body) {
    return this.request("POST", `/webauthn/login/begin`, { body });
  }
  /** Complete a WebAuthn login — optionally mint a token. */
  async postWebAuthnLoginFinish(query, body) {
    return this.request("POST", `/webauthn/login/finish`, { query, body });
  }
  /** Start a WebAuthn registration ceremony. */
  async postWebAuthnRegistrationBegin(body) {
    return this.request("POST", `/webauthn/registration/begin`, { body });
  }
  /** Complete a WebAuthn registration ceremony. */
  async postWebAuthnRegistrationFinish(query, body) {
    return this.request("POST", `/webauthn/registration/finish`, { query, body });
  }
};
export {
  SSOClient,
  SSOError
};
