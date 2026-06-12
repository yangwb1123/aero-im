-- User-level blocking/ignoring (user A blocks user B).
-- When user A blocks user B:
--   1. B's messages are filtered so A does NOT receive notifications from B.
--   2. A cannot open a DM with B (and vice versa).
CREATE TABLE user_blocks (
    blocker_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    blocked_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (blocker_id, blocked_id)
);
CREATE INDEX idx_user_blocks_blocked ON user_blocks (blocked_id);
