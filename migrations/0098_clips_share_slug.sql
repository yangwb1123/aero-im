-- 0098_clips_share_slug.sql — shareable URL slug for clips.
--
-- Adds an optional `share_slug` column to `stream_clips` so a clip can be shared
-- via a short, stable URL (e.g. /clips/abc123def456). The slug is generated
-- on-demand (POST /api/clips/:id/share) from the clip's own id bytes — no slug
-- means the clip is only accessible via its ULID id. The column is UNIQUE so two
-- clips can never share the same slug; the partial index makes the lookup fast
-- while ignoring NULL rows.
ALTER TABLE stream_clips ADD COLUMN share_slug TEXT UNIQUE;
CREATE UNIQUE INDEX clips_share_slug_idx ON stream_clips(share_slug) WHERE share_slug IS NOT NULL;
