-- 0091_ban_appeals.sql — ban / timeout appeal workflow.
--
-- A viewer banned (or timed-out) from a stream's danmaku chat (`stream_bans`, 0026)
-- may APPEAL: submit a reason for the creator/mods to review. The reviewer approves
-- (which lifts the ban via the existing unban path) or denies (the ban stands). One
-- row per appeal; a viewer may appeal again after a denial (a new row).
--
-- No FK to `stream_bans`: an appeal is a HISTORICAL/audit record that must OUTLIVE the
-- ban it appeals. Approving an appeal lifts the ban (DELETEs the `stream_bans` row), but
-- the appeal itself must persist as the `approved` record — a `(stream_id, appellant_id)`
-- FK with ON DELETE CASCADE would (wrongly) cascade the just-approved appeal away with the
-- ban inside the same transaction. (`stream_bans` has the COMPOSITE primary key
-- `(stream_id, participant_id)` and no surrogate id; `stream_moderators` is a DIFFERENT
-- table.) The ban's existence is instead enforced at SUBMIT time by the repository
-- (`BanAppealRepo::submit_appeal` rejects with `NotBanned` unless an active ban exists),
-- so no DB-level FK is needed or wanted here.
--
-- Purely additive — no existing table is touched. `reviewed_by` is a plain nullable
-- UUID column (the reviewing creator/mod participant), set on review.

CREATE TABLE IF NOT EXISTS ban_appeals (
    id              UUID        PRIMARY KEY,
    stream_id       UUID        NOT NULL,
    appellant_id    UUID        NOT NULL,
    appeal_reason   TEXT        NOT NULL,
    status          TEXT        NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'approved', 'denied')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    reviewed_by     UUID,
    reviewed_at     TIMESTAMPTZ,
    decision_reason TEXT
);

-- The creator/mod review queue: a stream's appeals, pending first / oldest first.
CREATE INDEX IF NOT EXISTS ban_appeals_stream_idx
    ON ban_appeals (stream_id, status, created_at ASC, id ASC);
