-- Optimistic-concurrency version for collaborative canvas edits (ROADMAP 第六版 ·
-- 方向五 协作安全). The canvas PUT was last-write-wins: two concurrent editors
-- silently overwrote each other. A monotonic `version` (bumped on every update)
-- lets a client send the version it read; the server rejects a stale version
-- (409) instead of silently clobbering the other edit. A lighter step than full
-- CRDT that eliminates the worst failure mode (silent lost updates).
--
-- Default 0 so existing rows + clients that don't send a version keep working
-- (the version check is skipped when no expected version is supplied).
ALTER TABLE channel_canvases ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 0;
