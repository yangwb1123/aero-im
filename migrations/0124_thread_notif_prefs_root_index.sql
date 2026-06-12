-- ROADMAP 方向二: index thread_notification_prefs for the batched per-root lookup.
-- The PRIMARY KEY (participant_id, root_message_id) leads with participant_id, so
-- the dispatch-time query `WHERE root_message_id = $1 AND participant_id = ANY($2)`
-- (one batch per threaded reply, replacing an O(R) get_level loop) cannot seek by
-- root and would scan. This composite index makes that lookup index-backed.
CREATE INDEX IF NOT EXISTS idx_thread_notif_prefs_root_participant
    ON thread_notification_prefs (root_message_id, participant_id);
