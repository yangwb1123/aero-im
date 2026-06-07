-- 0047 Revoked tokens (refresh-token revocation / logout).
--
-- Backs session management: `POST /api/auth/logout` records the SHA-256 hash of a
-- refresh token here, and `POST /api/auth/refresh` rejects any token whose hash is
-- present. Only the hash is stored (mirrors PAT / SCIM / webhook token handling),
-- so the table is useless if it leaks. `participant_id` is advisory (who revoked
-- it) and intentionally has no FK — a revocation outlives the participant row.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS revoked_tokens (
  token_hash text PRIMARY KEY,
  participant_id uuid,
  revoked_at timestamptz NOT NULL DEFAULT now()
);
