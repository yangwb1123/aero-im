# Runbook — Aero ID ↔ Aero IM account-summary target binding rollout

> Contract: `docs/designs/2026-08-29-aero-im-account-summary-target-binding-rollout.md`
> (IM side) and `aero-id/docs/contracts/source-account-summary.md` §"Aero IM
> target binding" (signer side). Deployment: `aero-id/deployments/k8s/README.md`.
> Status before owner rollout: **fail-closed on both sides** — no configuration,
> no route access, no account lookup.

## Owner rollout (ordered)

1. **Publish the dedicated JWKS** (owner-owned HTTPS endpoint, *not* this repo
   and *not* Snaplink's OIDC/integration JWKS): ED25519 (`OKP`/`crv=Ed25519`),
   `alg: EdDSA`, `use: sig`, `key_ops: [verify]`, unique explicit `kid`. During
   rotation publish old **and** new kids with overlap ≥ assertion lifetime +
   clock skew + JWKS cache TTL. Cleartext or redirecting endpoints are rejected
   by Aero IM at this boundary.
2. **Configure the Aero IM verifier**: `AERO__ACCOUNT_SOURCE__REGION` plus the
   `AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_{ISSUER,AUDIENCE,SUBJECT,JWKS_URI}`
   block (`.env.example` §"Aero ID account-summary target binding"). Audience
   must be `aero-im-account-summary`, subject `aero-id-sync`. Partial config is
   rejected at boot; an absent block keeps the route fail-closed (generic 502).
3. **Configure the Aero ID signer + OAuth** in the source `aero_im` block:
   OAuth client credentials (scope `aero.account.summary.read`, resource
   `aero-im`), `target_assertion_*` metadata, and the private key injected only
   via the Secret-backed env `AERO_ID_SOURCES_AERO_IM_TARGET_ASSERTION_PRIVATE_KEY`
   (key-env mode; key-file mode rejects symlinked volume paths). The OAuth
   client secret is Secret-only and never in a ConfigMap.
   Deploy both sides in the same window: the IM verifier rejects the
   general-integration JWKS/issuer, and a one-sided enable is not a rollout.

## Observable signals (existing, no new instrumentation)

| Signal | Meaning |
|---|---|
| Aero IM log `server error … account summary target binding is unavailable` + `aero_http_requests_total{method="GET",status="502"}` | Route reached but binding absent/unavailable — expected before step 2 lands; disappears once coordinated |
| Aero IM `401`/`403` on `/internal/account-summary` (debug log `client error`) | Invalid assertion (401) or target/header mismatch (403); none may occur on an approved roll |
| `aeroid_source_calls_total{source="aero-im",operation="summary"}` / `aeroid_source_errors_total{source="aero-im",operation="summary"}` | Connector attempts and failures; errors must drop to 0 after coordinated rollout |
| `aeroid_source_auth_compatibility_total{source="aero-im",mode="loopback"}` | Must stay **0** in any shared deployment; >0 means unauthenticated loopback mode was constructed |
| `aeroid_source_circuit_state{source="aero-im",…}` | Circuit state while the JWKS/OAuth path is exercised |

Hermetic verification (no external network) after each change:

```bash
# aero-im — verifier binding, HTTPS-only dedicated JWKS divergence, canonical vector
cargo test -p aero-server --lib account_summary
cargo test -p aero-server --lib integrations::tests
bash scripts/test-b5-pin-guard.sh && bash scripts/test-claim-contract-guard.sh
bash scripts/truth-check.sh

# aero-id — signer, config validation, env overrides, symlink rejection
go test ./internal/config ./internal/connector ./internal/service ./internal/app
go vet ./internal/config ./internal/connector ./internal/service ./internal/app
```

The cross-language test pair
`canonical_request_vector_is_stable_across_languages` (aero-im) and the
`account_summary_target*` protocol tests (aero-id) pin the shared canonical
request bytes/hash; they are the protocol-agreement gate and require no network.

## Rollback

Re-disable either side: unset the `AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_*`
block on Aero IM (route returns to fail-closed 502) or blank the
`sources.aero_im.target_assertion_*` block on Aero ID (signer off). Both are
safe independently because the wire protocol requires both a valid bearer and a
valid assertion.
