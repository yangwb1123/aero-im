# Account-summary target binding rollout gate

**Status: owner decision required; the Aero IM endpoint is fail-closed.**

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

`authorize_account_summary_target` in
`crates/aero-server/src/integrations.rs` is the explicit owner-facing seam. Its
checked-in implementation returns `AeroError::Upstream` with a generic
binding-unavailable message. When an owner-approved target is eventually
returned, the handler's following consistency check will compare any present
legacy headers to that verified target (malformed or mismatched values are
rejected). The account-summary handler calls this seam after machine
authentication and strict query parsing, but before `SsoRepo`,
`ParticipantRepo`, `WorkspaceRepo`, notification, projection, or audit reads.
There is no configuration or loopback exception that turns equal legacy headers
into authorization.

The request parser preserves `account_id` as an exact opaque string and never
converts it to `ParticipantId`. Legacy `X-Aero-Account-ID` and
`X-Aero-Canonical-UID` values, when present, are syntax/equality consistency
inputs only; they do not authorize a target.

## Activation contract still required

An owner-approved implementation must replace the non-accepting seam with a
verifier that returns an authorized target containing the exact opaque account
ID, canonical UID, tenant, source region, and effective dataset set. It must
bind those values to the validated machine client and the request, and must
fail closed on missing configuration, invalid signatures/claims, replay-store
or JWKS failure, mismatch, or expiry. It must not derive a participant ID from
an Aero ID account ID. Any present legacy headers may only be compared with the
verified target.

The owner must approve the issuer, audience, scope/resource spelling, signing
key/JWKS ownership and rotation, replay semantics, tenant uniqueness, region
routing, assertion lifetime, and legacy-route retirement before either
repository enables a successful request. In particular, the existing
`aero.account.summary.read` versus `account:summary:read` scope difference is
not silently reconciled here.

## Rollout

1. Keep the Aero IM route fail-closed and deploy no caller expecting a summary
   success yet.
2. Approve and version the target-binding protocol, then implement its verifier
   in Aero IM and trusted per-attempt assertion construction in Aero ID.
3. Add cross-language canonical request/hash vectors and negative tests for
   target, tenant, region, dataset, client, expiry, and replay mutation.
4. Only after the verifier and caller are deployed together, enable the route;
   remove any legacy header-only path rather than making the assertion optional.

Until those steps are complete, Aero ID summary calls receive a generic
upstream failure and no Aero IM account identity or projection lookup occurs.
