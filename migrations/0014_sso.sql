-- 0014_sso.sql — SSO via OIDC: external identity → internal participant mapping.
--
-- An OIDC ID-token login (POST /api/auth/oidc) validates an external IdP's token
-- and resolves it to an internal participant. This table is that mapping: a
-- (issuer, subject) pair from the IdP points at exactly one participant. On first
-- login for an unseen identity we JIT-provision a participant and link it here.
--
-- Purely additive — no existing table is touched. Idempotent (IF NOT EXISTS).

CREATE TABLE IF NOT EXISTS sso_identities (
    issuer         TEXT        NOT NULL,
    subject        TEXT        NOT NULL,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    email          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject)
);

-- Reverse lookup: all external identities linked to one participant (e.g. for
-- account-management / unlinking, and so the FK cascade has an index to follow).
CREATE INDEX IF NOT EXISTS sso_identities_participant_idx
    ON sso_identities (participant_id);
