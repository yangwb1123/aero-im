-- ROADMAP 方向一: index messages for the change-replay lookup.
-- A reconnecting client backfills NEW messages by id cursor (list_since), but
-- never learns of edits/deletes to messages it already holds. changes_since
-- queries `GREATEST(edited_at, deleted_at) > $since` per room — the latest
-- mutation instant (GREATEST ignores NULLs, so this is `edited_at > $since OR
-- deleted_at > $since`). This expression index makes that lookup index-backed
-- instead of scanning the room's whole history.
CREATE INDEX IF NOT EXISTS idx_messages_room_mutated
    ON messages (room_id, GREATEST(edited_at, deleted_at));
