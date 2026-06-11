-- 0092_predictions.sql — community predictions / channel betting (Twitch-style).
--
-- A "prediction" is a creator-opened bet on a live stream: the creator poses a
-- question with 2+ outcomes, viewers STAKE channel points on one outcome, the
-- creator LOCKS staking then RESOLVES to a winning outcome, and winners are paid
-- PROPORTIONALLY from the total pool. This is DISTINCT from `polls` (0027): a poll
-- is plain voting with no stakes and no payouts; a prediction debits/credits the
-- channel-points ledger (`points_ledger`, 0089) and settles a pool.
--
-- Lifecycle (`status`):
--   open      — accepting stakes.
--   locked    — staking closed; awaiting resolution (open -> locked).
--   resolved  — a winning outcome is set, payouts stamped + credited (terminal).
--   cancelled — the creator voided it; every staker is refunded (terminal).
--
-- Three additive tables:
--   * predictions          — the question + lifecycle + (on resolve) the winner.
--   * prediction_outcomes   — the 2+ labelled outcomes, keyed by a 0-based `idx`.
--   * prediction_stakes     — one row per (prediction, viewer) wager: the staked
--                             outcome + points, and the `payout` stamped on settle.
--
-- Settlement (in `PredictionRepo::resolve`): total_pool = sum(all stakes),
-- winning_pool = sum(stakes on the winning idx). If winning_pool = 0 (nobody
-- picked the winner) EVERY staker is REFUNDED their own stake; otherwise each
-- winner is paid floor(stake * total_pool / winning_pool) (losers get 0). The
-- multiply uses i128 in the repo to avoid bigint overflow. Staking debits
-- `points_ledger` for THIS prediction's creator (mirroring the atomic conditional
-- debit in `ChannelPointsRepo::redeem`); payout/refund credits it back.
--
-- Purely additive — no existing table is touched. `stream_id` / `creator_id` /
-- `viewer_id` are plain UUID columns (NOT cascading FKs to participants), mirroring
-- `goals` (0090) / `ban_appeals` (0091) so a prediction row outlives a pruned
-- stream/participant. `prediction_outcomes.prediction_id` /
-- `prediction_stakes.prediction_id` FK to `predictions(id)` ON DELETE CASCADE for
-- referential integrity WITHIN this feature.

CREATE TABLE IF NOT EXISTS predictions (
    id                  UUID        PRIMARY KEY,
    stream_id           UUID        NOT NULL,
    creator_id          UUID        NOT NULL,
    question            TEXT        NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'open'
                                    CHECK (status IN ('open', 'locked', 'resolved', 'cancelled')),
    -- Set only once the prediction resolves (NULL while open/locked/cancelled).
    winning_outcome_idx INT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at           TIMESTAMPTZ,
    resolved_at         TIMESTAMPTZ,
    expires_at          TIMESTAMPTZ
);

-- A stream's open predictions (the viewer-facing list) + the listing view, newest
-- first.
CREATE INDEX IF NOT EXISTS predictions_stream_idx
    ON predictions (stream_id, status, created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS prediction_outcomes (
    prediction_id UUID NOT NULL REFERENCES predictions(id) ON DELETE CASCADE,
    -- 0-based outcome index; the wire/stake refer to this.
    idx           INT  NOT NULL,
    label         TEXT NOT NULL,
    PRIMARY KEY (prediction_id, idx)
);

CREATE TABLE IF NOT EXISTS prediction_stakes (
    id            UUID        PRIMARY KEY,
    prediction_id UUID        NOT NULL REFERENCES predictions(id) ON DELETE CASCADE,
    outcome_idx   INT         NOT NULL,
    viewer_id     UUID        NOT NULL,
    points        BIGINT      NOT NULL CHECK (points > 0),
    -- Stamped on resolve/cancel: the points credited back (winnings or refund); 0
    -- for a loser. NULL while the prediction is still open/locked.
    payout        BIGINT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One stake per viewer per prediction — staking again is rejected (AlreadyStaked).
    UNIQUE (prediction_id, viewer_id)
);

-- The settlement scan (sum stakes per outcome) + a viewer's stake lookup.
CREATE INDEX IF NOT EXISTS prediction_stakes_prediction_idx
    ON prediction_stakes (prediction_id, outcome_idx);
