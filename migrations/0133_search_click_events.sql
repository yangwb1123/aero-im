-- Search click-feedback log (ROADMAP5 方向三 P2): the data foundation for
-- learning-to-rank / relevance analytics.
--
-- The advanced search returns a ranked result list but the system never learned
-- which result a user actually opened — so there was no CTR, no
-- mean-reciprocal-rank, no relevance signal to tune ranking with. This table
-- records one row per click-through: the query that produced the list, the
-- message that was opened, and its 0-based rank in that list. Aggregates
-- (click count, CTR vs. impressions, MRR) are computed over it.
--
-- No FK to messages (a clicked result may later be deleted/erased, and the
-- analytics signal should survive that). workspace_id scopes per-tenant rollups.
-- Append-only and swept by the data-lifecycle retention loop, like the other
-- high-volume event tables.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS search_click_events (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL,
    workspace_id   UUID        NOT NULL,
    -- The normalized query text whose result list was clicked (free-text terms).
    query_text     TEXT        NOT NULL,
    -- The message that was opened from the result list.
    result_id      UUID        NOT NULL,
    -- 0-based rank of the clicked result within the list (rank 0 = top hit).
    result_rank    INTEGER     NOT NULL,
    clicked_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-tenant, time-windowed rollups (CTR / MRR over a recent window) scan by
-- (workspace_id, clicked_at).
CREATE INDEX IF NOT EXISTS search_click_events_ws_time_idx
    ON search_click_events (workspace_id, clicked_at);
