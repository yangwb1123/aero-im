# Account-summary target binding rollout gate

**Status: implementation landed fail-closed; production activation still requires
owner-approved external JWKS deployment and coordinated rollout.**

## Decision and evidence

No already-approved account-to-identity binding contract was found in the
repositories reviewed for this change.

- `internal/service/sync_execute.go` in `/home/u1/aero-id` loads an account by
  `(job.TenantID, job.AccountID)` before calling a connector, but
  `internal/connector/http_summary_request.go` sends the account ID, canonical
  UID, tenant, and region as ordinary request values. The source-wide OAuth
  client-credentials token is not account-bound.
- `internal/pkg/sourceauth/policy.go` authenticates the source transport only;
  it does not authorize a summary target. The existing `EventIngestConfig`
  client/source binding is for canonical-event ingestion and is not a
  summary-account authorization mapping.
- Snaplink's `cmd/sso-server/serveraccount/account_summary.go` validates the
  service-token shape and scope, then looks up the caller-supplied canonical
  subject. Its account ID is not an authorization binding.
- No signed target assertion, assertion JWKS/replay contract, or durable
  client-account mapping is present for this summary path. The existing design
  memo's assertion proposal therefore remains a proposal, not an approved
  interoperable protocol.

Header/query equality cannot close this gap: it only proves that two
attacker-controlled request values are equal.

## Current Aero IM safety behavior

`AccountSummaryTargetVerifier` in
`crates/aero-server/src/account_summary_binding.rs` implements the proposed
owner-facing seam, but it is not enabled by default. Missing, partial, invalid,
or general-trust-domain configuration leaves the verifier absent and
`authorize_account_summary_target` returns a generic `AeroError::Upstream`
before any account or projection lookup. With the dedicated configuration,
the handler verifies the EdDSA assertion, binds its claims to the strict query
and validated machine client, checks the consistency headers, atomically
consumes the Redis replay key, and only then reaches `SsoRepo`,
`ParticipantRepo`, `WorkspaceRepo`, notification, projection, or audit reads.
There is no configuration or loopback exception that turns equal legacy headers
into authorization.

The request parser preserves `account_id` as an exact opaque string and never
converts it to `ParticipantId`. Legacy `X-Aero-Account-ID`,
`X-Aero-Canonical-UID`, tenant, and region values are required wire
consistency inputs after assertion verification; they do not authorize a
target.

## Activation contract still required

An owner-approved implementation must replace the non-accepting seam with a
verifier that returns an authorized target containing the exact opaque account
ID, canonical UID, tenant, source region, and effective dataset set. It must
bind those values to the validated machine client and the request, and must
fail closed on missing configuration, invalid signatures/claims, replay-store
or JWKS failure, mismatch, or expiry. It must not derive a participant ID from
an Aero ID account ID. Any present legacy headers may only be compared with the
verified target.

The implemented contract fixes these values for the bound path: EdDSA with
explicit `kid`; protected type `aero.account-target+jwt`; audience
`aero-im-account-summary`; subject `aero-id-sync`; scope
`aero.account.summary.read`; OAuth resource `aero-im`; canonical path
`/internal/account-summary`; sorted unique datasets; and one-time Redis replay
by issuer/JTI. Aero ID signs only after a tenant-scoped authoritative account
load, and retries mint a new JTI. The `reconcile=true` workflow flag is omitted
for Aero IM because it is not part of the strict IM query protocol.

The owner/deployment contract still covers the external dedicated JWKS
publication and rotation window, the issuer URL, the local source region, and
coordinated secret/config rollout. The Aero IM verifier rejects reuse of the
OIDC or general integration JWKS and remains absent on invalid/partial config.
The old `aero.account.summary.read` versus `account:summary:read` mismatch is
not silently reconciled.

## Rollout

1. Keep the Aero IM route fail-closed and deploy no caller expecting a summary
   success yet.
2. Approve and version the implemented target-binding protocol, publish the
   dedicated public JWKS, and deploy matching Aero ID signer/Aero IM verifier
   configuration together.
3. Keep the cross-language canonical request/hash vectors and negative tests
   for target, tenant, region, dataset, client, expiry, and replay mutation in
   the release gate.
4. Only after the verifier, caller, JWKS, and OAuth scope are coordinated,
   enable the route; remove any legacy header-only path rather than making the
   assertion optional.

Until the external JWKS and matching secrets/config are deployed, Aero ID
summary calls receive a generic target-binding failure and no Aero IM account
identity or projection lookup occurs. The checked-in example and Kubernetes
configuration leave target assertions disabled; enabling only one side is not
a valid rollout.

TARGET BINDING STATUS: FAIL_CLOSED_IMPLEMENTED_NEEDS_OWNER_DEPLOYMENT
