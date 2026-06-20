-- Failed login attempts (方向五 — 异常行为检测).
-- Separate from login_events (which only records *successful* logins)
-- so a slow-rolling credential-stuffing attack creates a detectable trail.
-- Rows are retained per the same sweep window as login_events.

CREATE TABLE IF NOT EXISTS login_failures (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The account identifier that was attempted (email, not participant_id,
    -- because the account may not exist — we detect enumeration too).
    account     TEXT NOT NULL,
    ip          TEXT,
    user_agent  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS login_failures_account_ts_idx
    ON login_failures (account, created_at DESC);

CREATE INDEX IF NOT EXISTS login_failures_ip_ts_idx
    ON login_failures (ip, created_at DESC);
