-- 0090_goals.sql — creator goal / bounty bars.
--
-- A "goal" is a streamer goal bar: a titled target on a metric the broadcast fills
-- toward as progress accrues (e.g. "100 gifts to unlock the boss fight"). The metric
-- is one of `gifts` / `viewers` / `points`; `current_tally` climbs toward `target`,
-- and the goal flips `active` -> `reached` the instant the tally first meets the
-- target. `cancelled` is the creator dropping a goal early.
--
-- Two additive tables:
--   * goals        — the goal definition + running tally + status.
--   * goal_events  — an append-only log of every progress delta, so the tally is
--                    reconstructable and the contribution history is auditable.
--
-- Purely additive — no existing table is touched. `stream_id`/`creator_id` are plain
-- UUID columns (NOT cascading FKs), mirroring `raid_history`/`stream_bans`, so a goal
-- row outlives a pruned stream. `goal_events.goal_id` is a plain UUID column too (the
-- progress path inserts an event in the same path as the tally bump; no FK so a
-- delta survives a goal pruned out of band).

CREATE TABLE IF NOT EXISTS goals (
    id             UUID        PRIMARY KEY,
    stream_id      UUID        NOT NULL,
    creator_id     UUID        NOT NULL,
    title          TEXT        NOT NULL,
    description    TEXT,
    metric_type    TEXT        NOT NULL
                               CHECK (metric_type IN ('gifts', 'viewers', 'points')),
    target         BIGINT      NOT NULL,
    current_tally  BIGINT      NOT NULL DEFAULT 0,
    status         TEXT        NOT NULL DEFAULT 'active'
                               CHECK (status IN ('active', 'reached', 'cancelled')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at     TIMESTAMPTZ
);

-- A stream's active goals (the gift path scans these per gift) + the listing view,
-- newest first.
CREATE INDEX IF NOT EXISTS goals_stream_idx
    ON goals (stream_id, status, created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS goal_events (
    id          UUID        PRIMARY KEY,
    goal_id     UUID        NOT NULL,
    delta       BIGINT      NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A goal's contribution timeline, oldest first.
CREATE INDEX IF NOT EXISTS goal_events_goal_idx
    ON goal_events (goal_id, created_at ASC, id ASC);
