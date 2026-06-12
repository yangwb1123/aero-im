-- Clip tagging system: user-defined free-text labels on stream_clips rows.
-- Each (clip_id, tag) pair is unique; tags are stored lowercased (enforced at the
-- application layer). The idx_clip_tags_tag index supports efficient tag-based
-- clip discovery (GET /api/clips?tag=xxx).
CREATE TABLE clip_tags (
    clip_id UUID NOT NULL REFERENCES stream_clips(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (clip_id, tag)
);
CREATE INDEX idx_clip_tags_tag ON clip_tags (tag);
