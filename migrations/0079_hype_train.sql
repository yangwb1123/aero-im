-- 0079_hype_train.sql — Hype Train / combo gifts (escalating momentum mechanic).
--
-- A "hype train" is an escalating momentum session on a live stream: rapid
-- successive gifts build "levels". Each session accumulates `contribution`
-- (gift units) within a time window; every N units advances the `level` by one
-- (up to a cap). The window slides forward on each contribution; when it lapses
-- without further contributions the session is `expired`; an owner/auto sweep may
-- mark it `completed`. The escalation state machine itself is pure Rust
-- (`aero_storage::hype_train`) — this table only persists the running totals.
--
-- Purely additive: two NEW tables; no existing table is reshaped, and the whole
-- script is idempotent (safe to re-run). `stream_id` is a plain column (not a
-- cascading FK) mirroring `stream_recordings`/`stream_clips`, so a session row can
-- outlive a pruned stream for analytics.
CREATE TABLE IF NOT EXISTS hype_train_sessions (
    id           UUID        PRIMARY KEY,
    stream_id    UUID        NOT NULL,
    level        INTEGER     NOT NULL DEFAULT 1,
    contribution INTEGER     NOT NULL DEFAULT 0,
    -- 'active' | 'completed' | 'expired'.
    state        TEXT        NOT NULL DEFAULT 'active',
    started_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL
);

-- "the current (active) session for this stream" lookup + per-stream history.
CREATE INDEX IF NOT EXISTS hype_train_sessions_stream_idx
    ON hype_train_sessions (stream_id, started_at DESC);

-- One row per (session, participant): who fed the train and how much, for the
-- per-train contributor breakdown. `units` accumulates across that participant's
-- gifts inside the session.
CREATE TABLE IF NOT EXISTS hype_train_contributions (
    session_id     UUID        NOT NULL REFERENCES hype_train_sessions(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL,
    units          INTEGER     NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, participant_id)
);
