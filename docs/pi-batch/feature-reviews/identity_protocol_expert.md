# Identity Protocol Expert Review — Aero IM

**Reviewer role**: identity-protocol reviewer (OAuth 2.0 / OIDC / CAEP-SSF / SAML / SCIM / WebAuthn).
**Scope note**: `prompts/README.md` does not exist in this repository (verified: no `prompts/` directory) — same situation the devops review reported. This review therefore applies the task's reviewer instructions directly against in-repo evidence: source inspection of every identity-touching module plus independent targeted test runs.
**Round-4 relationship**: the recall feature under review (B1/B2) does not touch the identity surface — no auth/SSO/SCIM/SAML/session code changed. This review re-verifies the identity-protocol stack as it stands and re-runs its hermetic suites: `cargo test -p aero-auth --lib` **81/0**, `cargo test -p aero-server --lib sso::tests` **16/0**, `saml::tests` **27/0**, `scim::tests` **25/0** (all green on the current dirty tree).
**Certification posture**: no OIDF/SCIM certification is claimed anywhere in the repo, and none is claimed in this review.

---

## 1. Protocol/profile scope and authoritative references

Aero IM is an **OIDC/SAML relying party** and a **SCIM 2.0 service provider** (inbound provisioning). It mints its own RS256 JWTs, manages TOTP 2FA, PATs, sessions, and HMAC-signed webhooks. It does **not** operate as an OIDC Provider, a CAEP/SSF transmitter/receiver, or a WebAuthn relying party.

| Protocol | Role in Aero | Authoritative references |
|---|---|---|
| OpenID Connect Core 1.0 | RP: authorization-code + PKCE browser flow; direct ID-token login API (proprietary); ID-token validation | `crates/aero-auth/src/oidc.rs`, `oidc/id_token.rs`, `crates/aero-server/src/sso.rs` | OIDC Core §§2, 3.1.2, 3.1.3 (esp. §3.1.3.7 ID-token validation, §3.1.2.6 `iss`), RFC 7636 (PKCE), RFC 7517 (JWK), RFC 7515/7519 |
| OAuth 2.0 (machine) | RFC 9068 `at+jwt` client-credentials token validation for Snaplink integrations | `aero-auth/src/oidc.rs::validate_client_credentials_token`, `crates/aero-server/src/integrations.rs` | RFC 9068 §§2.1–2.2, RFC 6749 §4.4, RFC 7519 |
| SAML 2.0 | SP: metadata, HTTP-Redirect AuthnRequest, HTTP-POST ACS; **XML-DSig verification fail-closed by default** | `crates/aero-server/src/saml.rs`, `saml/{conditions,request_state,signature_policy}.rs` | SAML Core 2.0 §§3.1–3.3; Bindings §§3.4–3.6; Metadata §2.4; Profiles §4.1; W3C XML-DSig + exclusive C14N |
| SCIM 2.0 | Service provider: Users + Groups CRUD, token auth, `eq` filter, pagination | `crates/aero-server/src/scim.rs`, `scim/{users,groups}.rs`, `aero-storage/src/scim.rs` | RFC 7643 (§§3.1, 4.1, 4.2), RFC 7644 (§§3.2, 3.4.2, 3.5.1, 3.5.2, 3.12) |
| TOTP 2FA | Second factor at login + self-management | `aero-auth/src/totp.rs`, `crates/aero-server/src/twofa.rs`, `aero-storage/src/totp.rs` | RFC 6238 (§§4–5), RFC 4226 §5.3, RFC 4648 (base32), RFC 2104 |
| First-party tokens | RS256 JWT access/refresh, sid binding, rotation + reuse detection, session inventory/revocation | `aero-auth/src/jwt.rs`, `service.rs`, `crates/aero-server/src/{session,sessions,session_control}.rs` | RFC 7519, RFC 9700 §4.14 (rotation), RFC 6819 §5.2.2.3 |
| PATs | `aero_pat_*` bearer credentials, hash-at-rest | `aero-auth/src/pat.rs`, `server/src/pat.rs`, `aero-storage/src/pat.rs` | RFC 6819 §5.1.4.1.3 (bearer-token storage) |
| Webhooks | Outgoing HMAC-SHA256 (`X-Aero-Signature`/`X-Aero-Timestamp`), incoming bearer-token URL | `aero-storage/src/webhook/{crypto,delivery}.rs`, `server/src/webhooks.rs` | RFC 2104; Slack-style signing-secret convention (variant) |
| Passwords | Argon2id PHC strings | `aero-auth/src/password.rs` | RFC 9106 |
| Session cookies | OIDC flow state/verifier/nonce cookies | `server/src/sso/transaction.rs` | RFC 6265 (HttpOnly), SameSite=Lax (6265bis §5.3.7) |
| **Explicitly out of scope (declared unsupported)** | CAEP/SSF (RFC 9501 family), WebAuthn/FIDO2, OIDC OP role, SAML SLO, SCIM Bulk/ETag/outbound sync | — | — |

---

## 2. Compliance matrix

Status legend: **PASS** = conformant as implemented · **DEVIATION** = implemented differently from the reference (direction noted) · **NOT-IMPLEMENTED** = declared seam/unsupported · **N/A** = role not applicable.

### 2.1 OIDC RP — ID-token validation (`aero-auth/src/oidc.rs`)

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| O1 | §3.1.3.7: `iss` MUST equal the configured issuer; `aud` MUST contain client_id; `exp` MUST be checked | `Validation::set_issuer/set_audience`, `set_required_spec_claims(&["iss","aud","exp","sub"])` | PASS | — | `rejects_wrong_issuer`, `rejects_wrong_audience`, `rejects_expired_token` |
| O2 | §3.1.3.7: `azp` — MUST be present and equal client_id **when multiple audiences**; when present must equal client_id | `SignedOidcClaims::into_verified` (cardinality-aware, `TokenAudience::Multiple`) | PASS | — | covered in oidc tests (81-suite) |
| O3 | §3.1.3.7: `nonce` — verify equals the value from the auth request | `sso.rs::verify_nonce` (constant-time) on browser flow | PASS | Direct `POST /api/auth/oidc` skips nonce — see **F1** | `callback_nonce_is_mandatory_and_exact` |
| O4 | §3.1.3.7: `sub` MUST be present and a valid identifier | required claim + `valid_identity_component` | PASS | — | — |
| O5 | §2: alg allowlist; no alg-confusion; JOSE `typ` policy | RS256/EdDSA only; `valid_id_token_type` (missing/JWT/application/jwt accepted; `at+jwt` rejected) | PASS | Stricter than spec (spec does not define `typ` for ID tokens) — safe direction | `rs256_id_token_type_policy_rejects_access_token_confusion`, `rejects_algorithm_outside_explicit_allowlist` |
| O6 | §3.1.3.7: `nbf` enforced when present; leeway bounded | `validate_nbf=true`, `LEEWAY_SECS=60` | PASS | — | — |
| O7 | §3.1.3.7: `iat` — freshness is not required by OIDC Core | `iat` optional, no max-age on the direct path | DEVIATION (acceptance) | See **F1** for the direct-login replay window | `ordinary_oidc_login_accepts_provider_without_iat` |
| O8 | RFC 7517 §4: JWKS key selection (`kid` exact match, `use`/`key_ops`/`alg` checks, no-kid ⇒ single key only) | `JwksKeyProvider`/`key_from_set`/`jwk_to_key` | PASS | `kid` duplicates ⇒ reject (rotation ambiguity); unknown-kid refresh single-flight throttled | `matches_key_by_kid`, `rejects_unknown_kid`, `unknown_kid_rotation_set_can_resolve_new_key_after_refresh`, `concurrent_unknown_kids_reserve_only_one_refresh` |
| O9 | §3.1.3.7: signature verification with the IdP key | `jsonwebtoken::decode` after allowlisted alg + key resolution | PASS | — | `rejects_bad_signature_from_different_key`, `rejects_malformed_token` |
| O10 | JWKS fetch hardening (SSRF/replay surface) | HTTPS-only (loopback HTTP for dev), no userinfo/query/fragment, redirects disabled, 256 KiB cap, 3s/8s timeouts, unknown-kid refresh ≤1/10s | PASS | — | `jwks_uri_policy_*`, `jwks_provider_rejects_unsafe_uri_before_network_fetch` |

### 2.2 OIDC RP — browser flow (`crates/aero-server/src/sso.rs`)

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| B1 | §3.1.2.1: auth request params (`response_type=code`, `scope`, `client_id`, `redirect_uri`, `state`, `nonce`) | `authorization_url`; reserved-param collision check on the configured endpoint | PASS | Scope fixed `openid profile email`; `redirect_uri` exact-match by construction | `authorization_redirect_has_state_nonce_and_pkce_s256` |
| B2 | RFC 7636 §4.2/§4.3: PKCE S256 challenge; verifier bound server-side and presented at token endpoint | `code_challenge=S256`, verifier in HttpOnly cookie, `code_verifier` in token request | PASS | — | same test |
| B3 | §3.1.2.6: `state` round-trip validation (CSRF) | 256-bit random state, constant-time compare, per-tab scoped cookie | PASS | — | `cookie_reader_rejects_duplicates_and_state_compare_is_exact`, `state_keyed_cookies_keep_parallel_browser_flows_independent` |
| B4 | §3.1.2.6: `iss` query param — MUST validate when present | `verify_authorization_issuer` (constant-time, when present) | PASS | Absent tolerated (spec permits when provider does not support it) | — |
| B5 | §3.1.3.2: token request — confidential-client auth (Basic), `redirect_uri` echoed, `grant_type=authorization_code` | `exchange_authorization_code` | PASS | — | — |
| B6 | §3.1.3.2/§3.1.3.3: token response — bounded, JSON-only consumption of `id_token` | 64 KiB cap, streaming read, `id_token` bounds | PASS | Content-Type not asserted (lenient accept) — INFO | — |
| B7 | §3.1.2.6: `error` response from provider → abort, never complete | `parse_callback_query` provider-error branch | PASS | — | `callback_query_rejects_duplicates_provider_errors_and_bad_state` |
| B8 | §3.1.3.7 nonce enforcement end-to-end (cookie → ID token) | `verify_nonce(&claims, &flow.nonce)` | PASS | — | `callback_nonce_is_mandatory_and_exact` |
| B9 | Cookie hygiene (RFC 6265 / SameSite) | `HttpOnly; Secure; SameSite=Lax; Path=/callback; Max-Age=900`; scoped-cookie model survives multi-tab + rolling deploy; legacy fixed-name cookies cleared only when state matches | PASS | — | `flow_cookies_are_short_lived_secure_and_http_only`, `callback_clears_only_the_matching_scoped_flow_plus_legacy_cookies` |
| B10 | Sensitive-response headers on the callback | `no-store`, `no-referrer`, `CSP default-src 'none'`, `X-Frame-Options: DENY`, `nosniff`; HTML payload JSON-escaped (`\u0026`/`\u003c`…) | PASS | — | — |
| B11 | Endpoint/redirect/URI validation (RFC 9700 §5.2.1 exact-match spirit) | HTTPS-only endpoints, no credentials/fragment, `redirect_uri` must be exactly the callback path, size caps everywhere | PASS | — | `browser_config_rejects_http_and_wrong_callback_path` |
| B12 | §3.1.3.7: JIT provisioning keyed on `(iss, sub)`; tombstone guard; session recorded before tokens released | `SsoRepo::resolve_or_provision_human`, `Tombstoned → Forbidden`, `SessionRepo::record_with_id` pre-release | PASS | `email_verified` never required — INFO (**F9**) | — |
| B13 | Anti-oracle: all validation failures collapse to 401/400 without leaking which check failed | `complete_oidc_login` maps every `OidcError` to categorical `Unauthorized`; `Debug` impl redacts claims | PASS | — | `oidc_claims_debug_redacts_identity_and_profile_values` |
| B14 | Rate limiting on auth entry points | `/api/auth/oidc`, `/api/auth/oidc/start` in `SENSITIVE_AUTH_PATHS` (auth_rate_limiter) | PASS | — | `rate_limit.rs` tests |

### 2.3 OAuth 2.0 machine identity — RFC 9068 (`integrations.rs`)

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| M1 | §2.1: JOSE `typ` MUST be `at+jwt` | `validate_client_credentials_token` typ gate | PASS | — | `rejects_client_credentials_token_with_wrong_typ` |
| M2 | §2.1: `iss`/`aud`/`exp`/`iat`/`jti`/`sub`/`client_id` claims | all in `required_spec_claims` (+`nbf`) | PASS | `nbf` required although RFC 9068 leaves it optional — fail-closed profile restriction | `rejects_missing_or_invalid_client_credentials_iat_and_jti` |
| M3 | §2.1: `sub` = `client_id` for client-credentials | enforced | PASS | — | `rejects_client_credentials_token_when_sub_differs_from_client_id` |
| M4 | §2.2 RS: scope check against configured required scopes | `granted_scopes` union of `scopes` array + `scope` string | PASS | — | `accepts_client_credentials_at_jwt_with_scopes_array`, `accepts_client_credentials_with_space_delimited_scope`, `rejects_client_credentials_token_without_required_scope` |
| M5 | §2.2 RS: reject `iat` in the future; `jti` usable | future-`iat` check, `jti` bounds (1..1024, no control chars) | PASS | — | `rejects_client_credentials_token_with_wrong_issuer_or_audience` |

### 2.4 SAML 2.0 SP (`crates/aero-server/src/saml.rs`)

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| S1 | Core §3.2.1: AuthnRequest (`ID`, `Version`, `IssueInstant`, `Destination`, `ProtocolBinding`, `ACS URL`, `Issuer`, `NameIDPolicy`) | `build_authn_request` | PASS | — | `authn_request_has_required_attributes` |
| S2 | Bindings §3.4: HTTP-Redirect encoding (raw DEFLATE → base64 → url-encode) | `redirect_url_for_authn_request` | PASS | — | `redirect_url_roundtrips_through_deflate_base64` |
| S3 | Metadata §2.4: EntityDescriptor/SPSSODescriptor, ACS (HTTP-POST, index 0, isDefault), `WantAssertionsSigned="true"`, NameIDFormats | `build_sp_metadata` | PASS | No `<KeyDescriptor>` / `AuthnRequestsSigned="false"` — see **F3** | `metadata_contains_entity_acs_and_wants_signed_assertions` |
| S4 | Bindings §3.5/§3.6: ACS POST form (`SAMLResponse`, `RelayState` ≤80 bytes), base64 per binding | `acs` handler, `decode_saml_response` (1.5 MiB b64 / 1 MiB XML caps) | PASS | — | `decode_saml_response_*` |
| S5 | Core §3.2.2: Response validation — Status Success, Destination, Issuer | `validate_response_conditions` | PASS | `Destination` on the Response required although SAML Core marks it optional — fail-closed; see **F14** | `response_conditions_reject_wrong_destination_or_recipient` |
| S6 | Core §3.3.2.2: `Conditions` NotBefore/NotOnOrAfter window | 90s skew, window ≤10 min enforced | PASS | 10-min cap is a profile restriction (some IdPs issue longer) — fail-closed | `response_conditions_apply_small_clock_skew_but_reject_expiry`, `response_conditions_reject_excessively_broad_validity_window` |
| S7 | Core §3.3.2.3: `AudienceRestriction` must include this SP | `validate_audiences` (required, each restriction must match) | PASS | — | `response_conditions_reject_wrong_audience` |
| S8 | Core §3.3.2.6: `OneTimeUse`; unknown signed conditions rejected | `validate_condition_elements` (fail-closed on anything else) | PASS | Stricter than spec — safe | `response_conditions_fail_closed_on_unknown_signed_conditions` |
| S9 | Core §3.3.2.1: SubjectConfirmation bearer — Method, Recipient, InResponseTo, NotOnOrAfter (+NotBefore when present) | `validate_response_conditions` | PASS | — | `response_conditions_enforce_subject_not_before`, `response_conditions_reject_mismatched_in_response_to` |
| S10 | **XML-DSig (W3C)**: signature verification of the assertion | `verify_response_signature` — **fail-closed by default**; opt-in `bergshamra` (un-audited) behind `AERO_SAML_EXPERIMENTAL_VERIFY=1` + feature flag; XSW invariant (exactly one Assertion, verified Reference covers it); profile preflight (exclusive C14N, RSA-SHA256, SHA-256, enveloped transform only, single Reference, URI = assertion ID, trusted-keys-only) | DEVIATION / NOT-IMPLEMENTED (production) | **The single most important posture in the stack**: no assertion is ever trusted by default; the staging seam is documented with a security caveat. Not a certification-grade SAML SP until a vetted verifier is wired | `signature_verification_is_fail_closed_and_rejects_everything`, `adversarial_*` (genuine/tampered/wrapping/wrong-cert/unsigned), `signature_policy` tests |
| S11 | Replay control: one-time AuthnRequest correlation | `request_state.rs` — Redis `SET NX` 5-min TTL at `/saml/login`, atomic `GETDEL` consume at ACS; unknown/expired/replayed → 401; Redis failure fails closed | PASS | SP-initiated only — unsolicited responses rejected (fail-closed deviation, documented) | `acs_consumes_request_before_any_jit_persistence`, `issued_request_is_consumed_exactly_once_under_race` (Redis-gated) |
| S12 | Identity extraction from the signed Assertion only | `extract_assertion`/`extract_signed_identity` — exactly one direct Assertion child, bounded NameID/attributes | PASS | — | `identity_extraction_ignores_unsigned_response_lookalikes`, `extract_assertion_*` |
| S13 | JIT provisioning + session hygiene | same `resolve_or_provision_human` + `SessionRepo::record_with_id` as OIDC; issuer exact-match | PASS | — | `config_from_env_requires_all_five_keys` |
| S14 | SLO / LogoutRequest / encrypted assertions / Artifact binding | — | NOT-IMPLEMENTED | declared | — |

### 2.5 SCIM 2.0 (RFC 7643/7644)

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| C1 | RFC 7643 §4.1/§4.2: User/Group core schema (schemas URNs, `id`, `externalId`, `userName`, `name`, `emails`, `active`, `meta`; Group `displayName`/`members`) | `ScimUser`/`ScimGroup` serde with camelCase renames, `default_true` for `active` | PASS | `emails` output derived from `userName` when email-shaped (best-effort projection) | `scim_user_roundtrips_camelcase`, `scim_user_deserializes_minimal_okta_payload`, `scim_group_roundtrips_camelcase` |
| C2 | RFC 7644 §3.2: request auth — bearer credential scoped to a tenant | per-workspace SCIM token, hash-at-rest, quota, admin-gated mint/revoke; token can never touch another tenant | PASS | — | `scim_token_quota_maps_to_explicit_conflict`, `scim_token_authz_errors_keep_forbidden_and_not_found_statuses` |
| C3 | RFC 7644 §3.4.2: ListResponse (`totalResults`, `startIndex`, `itemsPerPage`, `Resources`) | `ScimListResponse`; 1-based `startIndex` (0/negative → 1 per RFC) | PASS | `count=0`/negative clamped to 200 rows — see **F2** | `list_response_roundtrips_camelcase` |
| C4 | RFC 7644 §3.4.2.2: filtering | `userName eq "x"` only (strict parser); **unsupported/unparseable filter ⇒ list-all** | DEVIATION | See **F2** (superset response) | `parse_filter_*` |
| C5 | RFC 7644 §3.5.1: PUT full replacement | `put_user` → `update_user_atomic` | PASS | — | — |
| C6 | RFC 7644 §3.5.2: PATCH `Operations` (`op`/`path`/`value`) | Users: `replace`/`add` on `active`/`userName`/`externalId`/`name.formatted`/`displayName`; whole-resource replace; **`remove` and unknown ops silently ignored**; Groups: member add/remove (plain + filtered path), displayName replace | DEVIATION | Full path-filter grammar not implemented (documented seam, matches AGENTS.md); silent ignore can mask IdP-side misconfigurations — see **F4** | `scim_patch_operations_are_bounded`, `group_patch_*` |
| C7 | RFC 7644 §3.12: Error response (`schemas`, `detail`, `status` as **string**) | `ScimError`; every failure renders SCIM-shaped, not the gateway error shape | PASS | — | `scim_error_status_is_a_string` |
| C8 | RFC 7644 status-code semantics | 201 create / 200 read-update / 204 delete / 404 / 409 (unique violation) | PASS | — | `scim_identity_lifecycle_conflicts_map_to_fixed_http_conflicts` |
| C9 | Deprovisioning semantics: `active:false` reversible fence; DELETE final deprovision | `set_active`, `delete_user` (transactional, tenant-scoped, never deletes the global participant) | PASS | — | — |
| C10 | Group membership tenant hygiene | members must be members of the token's workspace (`groups.rs:150`), group must belong to the workspace | PASS | — | — |
| C11 | RFC 7644 §3.4.2.2/§3.4.3/§3.10-3.11: full filter grammar, Bulk, ETag/versioning, `schemas` extension attributes, nested groups, password writeback, outbound sync | — | NOT-IMPLEMENTED | declared | — |

### 2.6 TOTP / 2FA (RFC 6238) and first-party tokens

| # | Spec section / requirement | Evidence | Status | Deviation / notes | Test |
|---|---|---|---|---|---|
| T1 | RFC 6238 §4: T0=0, X=30, 6 digits, HMAC-SHA1; RFC 4226 §5.3 dynamic truncation | `aero-auth/src/totp.rs` | PASS | Canonical RFC 6238 Appendix B vector passes | `current_code_matches_rfc6238_sha1_vector` |
| T2 | RFC 6238 §5.2: clock-skew tolerance | ±1 step; constant-time compare; malformed codes rejected | PASS | — | `verify_accepts_current_and_one_step_skew`, `verify_rejects_far_skew_and_wrong_code` |
| T3 | Enrollment/activation/disable lifecycle | pending → verify → activate; disable requires current code when activated (hijack-resistant); recovery-code batch requires 2FA + code | PASS | — | twofa tests |
| T4 | Recovery codes: one-time use, hash-at-rest, TOCTOU-safe | atomic `UPDATE … WHERE used_at IS NULL RETURNING` | PASS | — | recovery-code tests |
| T5 | Login 2FA enforcement + lockout interplay | password → TOTP/recovery gate; 2FA failure counted on the lockout (deferred success), durable failure trail | PASS | — | auth tests |
| T6 | TOTP replay within window | no per-secret last-counter tracking | DEVIATION (acceptance) | RFC 6238 does not require one-time enforcement; a captured code is usable ≤90s and concurrently — INFO (**F7**) | — |
| J1 | First-party JWT: RS256, `kid` stamping, keyring rotation, per-token `jti` | `jwt.rs` — zero-downtime rotation with retained verifiers | PASS | — | `kid_rotation_verifies_old_tokens_with_retained_verifier`, `same_second_tokens_are_distinct` |
| J2 | RFC 9700 §4.14.2: refresh-token rotation + reuse detection | `session.rs` — in-place DB rotation, revoked-hash blacklist, reuse ⇒ whole session family revoked (10s lost-response grace) | PASS | 10s grace is a documented deliberate deviation from strict reject-on-reuse | session tests |
| J3 | Revocability: sid-bound access tokens, session inventory, revoke-one/others/admin-global | `sessions.rs`, `admin_sessions.rs` (workspace-owner-gated), NATS `publish_revoke_*` for cross-instance | PASS | sid-less access tokens hard-rejected | `sidless_access_claims_fail_before_session_lookup` |
| P1 | PAT: 256-bit entropy, prefix-gated, hash-at-rest, one-time plaintext, revoke, expiry clamp | `aero-auth/src/pat.rs`, `server/src/pat.rs`, `storage/src/pat.rs` | PASS | — | `well_formed_pat_*` |
| W1 | Outgoing webhook signing: HMAC-SHA256 over `{timestamp}.{body}`, `v0=` hex, freshness timestamp | `storage/webhook/crypto.rs` (openssl-vector-verified test), `delivery.rs` | PASS | Custom scheme modeled on Slack's convention (separator differs: `.` vs `:`); receivers must implement the matching verify — documented | `repo.rs` signing tests |
| W2 | Incoming webhook auth: token in URL, hash-at-rest, unknown/revoked ⇒ uniform 404 | `server/src/webhooks.rs::incoming_post` | PASS | Token-in-path may appear in access logs — INFO (**F8**) | — |
| A1 | IP allowlist enforcement (trusted-proxy-aware IP, fail-open on DB error, anti-lockout route exemption, SCIM covered, path-scoped second boundary) | `server/src/ip_allowlist.rs::enforce_layer` | PASS | Fail-open stance is documented and deliberate | ip_allowlist tests |

---

## 3. Findings

Requirement levels: **MUST/SHOULD/MAY** per the referenced spec; severity is this reviewer's risk assessment.

### F1 — MEDIUM — Direct ID-token login (`POST /api/auth/oidc`) has no replay binding
- **Requirement**: OIDC Core §3.1.3.7 (nonce binding — in spirit; the endpoint is proprietary, no auth request exists); RFC 9700 §4.7 (bearer-token replay).
- **Location**: `crates/aero-server/src/sso.rs` — `oidc_login` → `complete_oidc_login(..., /* expected_nonce */ None)`; `validate_id_token` treats `iat` as optional with no max-age.
- **Impact**: any captured IdP-signed ID token for this audience (client-side logs, a non-TLS client leg, malware on the SDK host) can be POSTed verbatim to authenticate as the victim for the token's remaining validity (typically 5–10 min at major IdPs). The auth rate limiter throttles volume but not a single replay. The browser flow is immune (nonce + PKCE + state), so the exposure is confined to the SDK/direct path — but that path is a documented login surface.
- **Corrective behavior**: (a) require `nonce` on the direct path too (SDK includes the nonce it used in its own auth request, or the endpoint is documented as SDK-only), (b) enforce a short `iat` freshness window (e.g. reject `iat < now - 300s`) when the direct path is enabled, or (c) deprecate the direct path in favor of code+PKCE. At minimum, document the replay window in the endpoint doc comment.

### F2 — MEDIUM — SCIM query: unsupported filter ⇒ full-user-list superset; `count=0`/negative ⇒ 200 rows
- **Requirement**: RFC 7644 §3.4.2.2 (unsupported/invalid filter handling — the RFC's signaled response is `400` with `scimType: invalidFilter`; silently returning a superset is the worst of the allowed behaviors) and §3.4.2.4 (`count=0` SHALL mean no results; negative SHALL be interpreted as 0).
- **Location**: `crates/aero-server/src/scim/users.rs::list_users` (unparseable filter ⇒ `filter_user_name=None` ⇒ list-all); `crates/aero-storage/src/scim.rs::clamp_count` (0/negative ⇒ `MAX_PAGE=200`).
- **Impact**: an IdP whose `userName eq` value trips the strict parser (escaped quotes, trailing whitespace clause, non-ASCII) receives the **entire workspace directory** instead of one row — a response superset that breaks reconciliation and, if a SCIM token is ever leaked, turns it into a directory dump. `count=0` callers get 200 rows.
- **Corrective behavior**: return `400` + `scimType: invalidFilter` for unparseable/unsupported filters (or, if list-all on unsupported *operators* is desired, keep it only for filters that parse cleanly); implement RFC count semantics (`0` → empty page, negative → 0 rows) with an upper clamp.

### F3 — LOW — SAML SP metadata: no `KeyDescriptor`, `AuthnRequestsSigned="false"`, unsigned requests
- **Requirement**: SAML Metadata §2.4.1 / Core §3.2.1 — request signing is optional (MAY), so this is compliant; it is an **interop** finding.
- **Location**: `crates/aero-server/src/saml.rs::build_sp_metadata` / `build_authn_request`.
- **Impact**: some enterprise IdPs (ADFS, some Shibboleth configs) require a signed `AuthnRequest` or an SP signing certificate at registration; deployments behind such IdPs will fail at IdP registration, not at runtime.
- **Corrective behavior**: when an SP signing key is configured, emit `<KeyDescriptor use="signing">`, set `AuthnRequestsSigned="true"`, and sign the `AuthnRequest` (HTTP-Redirect `SigAlg`/`Signature` params); otherwise document the no-signing-cert registration requirement.

### F4 — LOW — SCIM PATCH silently ignores `remove` and unknown ops/paths
- **Requirement**: RFC 7644 §3.5.2 (a server that cannot apply an operation SHOULD surface the error rather than silently succeed).
- **Location**: `crates/aero-server/src/scim/users.rs::patch_user` (`remove` and unknown ops `continue`); `scim/groups.rs` documented-seam skips.
- **Impact**: an IdP sending `remove: active` (or a typo'd path) gets `200` with no change — the IdP believes provisioning applied. Silent success masks integration bugs and makes reconciliation harder.
- **Corrective behavior**: respond `400` with `scimType: invalidPath`/`noTarget` for unsupported paths/ops (while keeping the documented partial grammar), or at minimum return the op-apply report in the response for audit.

### F5 — LOW — OIDC token-response Content-Type not asserted
- **Requirement**: OIDC Core §3.1.3.3 (token response is JSON; clients MUST accept JSON) — lenient accept is not a violation, but unlabeled bodies can mask a captive portal / wrong endpoint.
- **Location**: `sso.rs::exchange_authorization_code` (parses JSON without checking `Content-Type`).
- **Impact**: negligible security impact (body is size-bounded and JSON-parsed); a wrong-endpoint misconfiguration yields a confusing "invalid code" instead of a config error.
- **Corrective behavior**: warn-log (not reject) when `Content-Type` is not `application/json`.

### F6 — INFO — SAML ACS requires `Destination` on the Response (Core marks it optional)
- **Requirement**: SAML Core §3.2.2 — `Destination` MAY be present on `Response`.
- **Location**: `saml/conditions.rs::validate_response_conditions`.
- **Impact**: fail-closed deviation; IdPs that omit Response-level `Destination` (relying only on `SubjectConfirmationData Recipient`) will be rejected. Interop note, not a defect — the check is a genuine replay/misdelivery control.
- **Corrective behavior**: document; if interop demands, accept absence only when `SubjectConfirmationData Recipient` matches (keep the current strictness as the default).

### F7 — INFO — TOTP codes reusable within the acceptance window; no session binding
- **Requirement**: RFC 6238 does not mandate one-time enforcement.
- **Location**: `aero-auth/src/totp.rs::verify` (no last-counter tracking), `auth_login` (code not tied to the session).
- **Impact**: an intercepted code is valid for up to ~90s and can be used from multiple concurrent clients. Standard TOTP practice (Google/Microsoft behave the same); flag for product awareness only.

### F8 — INFO — Inbound webhook bearer token lives in the URL path
- **Requirement**: OAuth bearer-token hygiene (RFC 6750 §2.3 discourages URI-embedded credentials — but inbound webhooks are a server-to-server integration credential, not a user bearer token).
- **Location**: `POST /hooks/in/:token`.
- **Impact**: the token appears in gateway/proxy access logs. At-rest only the SHA-256 hash is stored; 404 hides existence.
- **Corrective behavior**: document the logging consideration; optionally support an `Authorization: Bearer <token>` variant.

### F9 — INFO — `email_verified` is passed through but never required for JIT provisioning
- **Requirement**: OIDC Core §5.1 note — RPs should only trust verified email for account linking; here the account identity is `(iss, sub)`, email is cosmetic profile data.
- **Location**: `sso.rs::complete_oidc_login` → `resolve_or_provision_human(email=…)`; `saml.rs::acs`.
- **Impact**: none on authentication binding (identity is the signed subject); only display-name/email decoration could carry an unverified value. Product decision — document.

### F10 — INFO — OpenAPI document is illustrative and omits the entire identity surface
- **Requirement**: n/a (no normative OpenAPI contract is claimed — AGENTS.md explicitly calls it 手写示意性文档).
- **Location**: `crates/aero-server/src/openapi.rs` — describes only messages/rooms/auth/me/streams; no SSO/SCIM/SAML/PAT/session/webhook paths; `securitySchemes` declares only JWT bearer (PATs and SCIM tokens are alternative bearer credentials).
- **Impact**: API consumers tooling against `/api/openapi.json` will not discover the identity endpoints and may mis-assume one auth scheme.
- **Corrective behavior**: add the identity endpoints and the PAT/SCIM bearer schemes, or mark the document `x-illustrative: true` and point to the README matrix.

---

## 4. Priority conformance tests, declared unsupported features, certification evidence

### 4.1 Priority conformance tests (author next; the suites re-run today are green but do not yet cover these)

1. **OIDC replay / nonce**: (a) after F1 fix — POST the same valid ID token twice to `/api/auth/oidc`; second must fail; (b) ID token with `iat` older than the freshness window; (c) browser-flow callback with an ID token whose `nonce` belongs to another flow (covered by `callback_nonce_is_mandatory_and_exact` — extend to a *stale* nonce from a consumed flow).
2. **OIDC downgrade/oracle**: `alg: none`, `HS256`, `RS384`, unknown `kid` with a JWKS that rotates mid-request; `typ: at+jwt` presented to the ID-token path and vice versa (mostly covered in the 81-suite; add a live `JwksKeyProvider` fetch-path test with a local HTTP server, since the network seam is the documented untested part).
3. **SAML**: (a) once the bergshamra feature is enabled — replay the **same** signed `SAMLResponse` twice against ACS (Redis-gated, `GETDEL`); (b) XSW with two assertions where the *first* is signed and the second consumed (covered by `adversarial_signature_wrapping_is_rejected` — keep as a gate); (c) Response without `Destination` (F6 policy decision); (d) IdP-initiated unsolicited response (must 401).
4. **SCIM**: `count=0` and `count=-5` (expect empty per RFC 7644 §3.4.2.4 after F2); `filter=userName eq "x" and active eq true` (expect explicit 400/filtered, not list-all); PATCH `remove` op on a supported path; `startIndex` beyond total (empty `Resources`, correct `totalResults`).
5. **Refresh reuse**: rotate, then replay the old refresh token after the 10s grace (session family revoked, access tokens dead) — confirm the existing tests cover the >10s branch; add a clock-boundary test at exactly 10s.
6. **Webhook receiver verification reference**: publish the `openssl dgst -sha256 -hmac` verification vector (exists in `webhook/repo.rs` tests) as the documented receiver contract.

### 4.2 Declared unsupported features (documented seams, not defects)

- **SAML**: production XML-DSig verification (fail-closed; opt-in experimental `bergshamra` is explicitly un-audited), SLO/LogoutRequest, IdP-initiated (unsolicited) SSO, request signing, encrypted assertions, Artifact binding, >10-min assertion windows.
- **OIDC**: OP role (no `.well-known/openid-configuration`, no JWKS publication — RP-only), hybrid/implicit flows, `form_post` response mode, PAR, DPoP, FAPI/JARM, back-channel logout. The `GET /api/auth/config` document is proprietary, not OIDC Discovery.
- **SCIM**: full filter grammar, full RFC 7644 §3.5.2 path-filter grammar, Bulk, ETag/versioning, extension schemas, password writeback, nested groups, outbound sync.
- **Others**: CAEP/SSF (RFC 9501) — no continuous-access-evaluation or shared-signals code or scaffold; WebAuthn/FIDO2 — no endpoints; OAuth authorization-server role — no `/authorize`/token issuance to third parties.

### 4.3 Certification evidence

- **No OIDF, OIDC, SAML, or SCIM certification is claimed or evidenced anywhere in the repository**, and none is claimed here. Certification would require: a vetted (audited) SAML XML-DSig verifier (F-path), SCIM full PATCH/filter conformance or a formal partial-profile declaration, OIDC conformance-suite runs against the browser flow, and published results — none exist.
- What *is* evidenced today: hermetic conformance-vector tests (RFC 6238 Appendix B vector; RFC 4226 dynamic truncation; JWKS key-selection edge cases; XSW adversarial tests; RFC 9068 claim matrix), `aero-auth` 81/0, `sso` 16/0, `saml` 27/0, `scim` 25/0, plus the full workspace suite (2159/0 per the round-4 evidence) — strong engineering evidence, not certification.

### 4.4 Bottom line

The identity stack is defense-in-depth sound where it is *enabled*: OIDC RP validation is spec-faithful (azp, nonce, PKCE, alg/typ allowlists, JWKS hardening, anti-oracle error collapse), refresh-token rotation with reuse detection and sid-bound revocable access tokens exceed typical practice, TOTP is vector-exact, and the SAML ACS is fail-closed with an unusually complete structural condition check. The three actionable items are **F1** (direct ID-token replay binding), **F2** (SCIM filter/count semantics), and **F3** (SAML SP signing metadata for IdP interop); the SAML signature-verification seam (**S10**) remains the single largest gap between "staging" and "production" for the SAML profile, and it is honestly documented as such.
