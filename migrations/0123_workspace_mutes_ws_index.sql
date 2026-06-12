-- ROADMAP 方向二: index workspace_mutes for the batched suppression lookup.
-- The table's PRIMARY KEY (participant_id, workspace_id) leads with participant_id,
-- so the dispatch-time query `WHERE workspace_id = $1 AND participant_id = ANY($2)`
-- (one batch per @everyone, replacing an O(N) is_muted loop) cannot seek by
-- workspace and would scan. This composite index makes that lookup index-backed.
CREATE INDEX IF NOT EXISTS idx_workspace_mutes_ws_participant
    ON workspace_mutes (workspace_id, participant_id);
