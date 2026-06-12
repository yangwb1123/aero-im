-- Migration 0103: clip view count tracking
-- Adds a view_count column to stream_clips, incremented each time a clip is fetched.

ALTER TABLE stream_clips ADD COLUMN view_count BIGINT NOT NULL DEFAULT 0;
