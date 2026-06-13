-- Login IP/device history (ROADMAP5 方向五): the persistent record the
-- in-process failed-login lockout (login_throttle) couldn't provide.
--
-- One row per SUCCESSFUL login, capturing the source IP and user-agent, so the
-- system can (a) show a user their recent login activity, (b) flag a login from
-- an IP that account has never used before (the canonical account-takeover
-- signal), and (c) later feed an impossible-travel / geo-velocity check (the geo
-- resolution itself needs a geo-IP database — a deployment seam — so this table
-- is the schema foundation that hook builds on).
--
-- Append-only and swept by the data-lifecycle retention loop. No FK-cascade
-- dependence beyond participant_id (a deleted participant's login history is
-- erased with them).
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS login_events (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- Source IP as a string (proxy-forwarded); NULL when not derivable.
    ip             TEXT,
    -- Client user-agent; NULL when absent.
    user_agent     TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "Recent logins for this user" + "has this user logged in from this IP before"
-- both scan by participant; the second filters on ip too.
CREATE INDEX IF NOT EXISTS login_events_participant_idx
    ON login_events (participant_id, created_at DESC);
