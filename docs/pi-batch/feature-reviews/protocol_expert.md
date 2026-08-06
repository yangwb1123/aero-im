Review complete — written to `docs/pi-batch/feature-reviews/identity_protocol_expert.md`.

## Summary

**Scope note**: `prompts/README.md` does not exist (no `prompts/` dir) — same finding as the devops review; I applied the task's reviewer instructions against in-repo evidence and re-ran the hermetic identity suites: **aero-auth 81/0, sso 16/0, saml 27/0, scim 25/0** (all green). The round-4 recall feature (B1/B2) touches no identity code, so this re-verifies the stack as it stands.

**What's genuinely strong** (PASS rows):
- **OIDC RP**: spec-faithful ID-token validation (azp multi-audience rule, nonce, required iss/aud/exp/sub, nbf, 60s leeway), RS256/EdDSA-only alg allowlist with `typ` confusion guards, hardened JWKS (HTTPS-only, no redirects, kid exact-match, unknown-kid single-flight throttle), PKCE S256 + state + nonce all bound in HttpOnly/Secure/SameSite=Lax scoped cookies, anti-oracle error collapse, RFC 9068 `at+jwt` machine validation (sub==client_id, scopes, jti, future-iat rejection).
- **SAML SP**: fail-closed XML-DSig gate (correct posture — un-audited bergshamra is opt-in and caveated), unusually complete condition checking (Destination, InResponseTo both levels, ≤10-min window, AudienceRestriction, OneTimeUse, bearer Recipient), Redis GETDEL one-time request consumption, XSW invariant.
- **Tokens**: sid-bound revocable access JWTs, refresh rotation **with reuse detection** (10s grace → whole family revoked), TOTP RFC 6238 vector-exact, hashed recovery codes with atomic one-time consume, PAT 256-bit entropy hash-at-rest.

**Findings (3 actionable)**:
- **F1 MEDIUM** — `POST /api/auth/oidc` (direct ID-token login) has no nonce/replay binding; a captured ID token replays for its validity window. Fix: require nonce and/or short `iat` freshness on that path.
- **F2 MEDIUM** — SCIM unsupported/unparseable filter returns the *entire workspace directory* (superset), and `count=0`/negative returns 200 rows instead of RFC 7644 §3.4.2.4 semantics. Fix: `400 invalidFilter` + RFC count semantics.
- **F3 LOW** — SAML metadata has no `KeyDescriptor` and requests are unsigned; some IdPs (ADFS) will fail registration. Plus LOW F4 (SCIM PATCH silently ignores `remove`), INFO F5–F10 (token-response Content-Type, Response-level Destination strictness, TOTP window reuse, token-in-URL webhooks, `email_verified` unused, OpenAPI omits the identity surface).

**Certification**: none claimed in-repo, none claimed here. The SAML production signature-verification seam remains the single biggest gap between staging and production for that profile, and it is honestly documented as such.
