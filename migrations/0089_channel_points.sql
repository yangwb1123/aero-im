-- 0089_channel_points.sql — channel points + custom-reward redemption.
--
-- Twitch-style "Channel Points": a viewer accrues points with a creator (watching,
-- gifting, etc.), then spends them to redeem a creator-defined custom reward. Four
-- additive tables:
--
--   * points_ledger        — the running balance per (viewer, creator). The PK is
--                            the pair, so each viewer holds an independent balance
--                            with each creator (balances never cross-pollinate).
--   * points_earn_history  — an append-only audit of every +/- delta with a reason
--                            (earn or spend), so a balance is reconstructable.
--   * reward_definitions   — a creator's catalog of redeemable rewards (title +
--                            point cost; `auto_fulfill` skips the queue, `enabled`
--                            hides a retired reward without deleting its history).
--   * redemption_queue     — one row per viewer redemption, `pending` until the
--                            creator/mods resolve it to `fulfilled` / `rejected`.
--
-- Purely additive — no existing table is touched. `viewer_id`/`creator_id` are plain
-- UUID columns (NOT cascading FKs to participants): mirrors `stream_bans`/`raid_history`
-- so a ledger row outlives a pruned participant. `redemption_queue.reward_id` FKs to
-- `reward_definitions(id)` for referential integrity within this feature.

CREATE TABLE IF NOT EXISTS points_ledger (
    viewer_id   UUID   NOT NULL,
    creator_id  UUID   NOT NULL,
    balance     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (viewer_id, creator_id)
);

CREATE TABLE IF NOT EXISTS points_earn_history (
    id          UUID        PRIMARY KEY,
    viewer_id   UUID        NOT NULL,
    creator_id  UUID        NOT NULL,
    -- Signed: positive on earn, negative on spend.
    delta       BIGINT      NOT NULL,
    reason      TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A viewer's earn/spend timeline with a creator, newest first.
CREATE INDEX IF NOT EXISTS points_earn_history_pair_idx
    ON points_earn_history (viewer_id, creator_id, created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS reward_definitions (
    id            UUID        PRIMARY KEY,
    creator_id    UUID        NOT NULL,
    title         TEXT        NOT NULL,
    cost          BIGINT      NOT NULL,
    auto_fulfill  BOOLEAN     NOT NULL DEFAULT false,
    enabled       BOOLEAN     NOT NULL DEFAULT true,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A creator's reward catalog listing (enabled first / newest first at read time).
CREATE INDEX IF NOT EXISTS reward_definitions_creator_idx
    ON reward_definitions (creator_id, created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS redemption_queue (
    id           UUID        PRIMARY KEY,
    reward_id    UUID        NOT NULL REFERENCES reward_definitions(id) ON DELETE CASCADE,
    viewer_id    UUID        NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending', 'fulfilled', 'rejected')),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at  TIMESTAMPTZ
);

-- The creator/mod redemption-queue view: a reward's redemptions, oldest first
-- (FIFO fulfillment), filtered by status when the UI shows only pending claims.
CREATE INDEX IF NOT EXISTS redemption_queue_reward_idx
    ON redemption_queue (reward_id, status, created_at ASC, id ASC);
