-- 0044 TOTP secrets (RFC 6238 authenticator-app two-factor authentication).
--
-- One row per participant who has enrolled an authenticator app. Enrollment
-- stores the shared base32 secret with `activated = false`; the participant then
-- proves possession by submitting a current code, which flips `activated` true.
-- Re-enrolling overwrites the secret and resets activation. The login-time
-- enforcement (require a valid code when `activated`) is wired in the auth path;
-- this table only owns the per-participant secret + activation state.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS totp_secrets (
  participant_id uuid PRIMARY KEY,
  secret text NOT NULL,
  activated boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now(),
  activated_at timestamptz
);
