-- Bound password-reset token history without affecting any currently usable
-- token. The retention worker deletes only rows whose expiry/usage timestamp is
-- older than its configured cutoff.

CREATE INDEX IF NOT EXISTS password_reset_tokens_expires_at_idx
    ON password_reset_tokens (expires_at);

CREATE INDEX IF NOT EXISTS password_reset_tokens_used_at_idx
    ON password_reset_tokens (used_at)
    WHERE used_at IS NOT NULL;
