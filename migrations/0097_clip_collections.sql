-- 0097_clip_collections.sql — clip collections / playlists.
--
-- A participant can group stream clips into named, ordered collections (like a
-- YouTube playlist). The collection itself is metadata (title + optional
-- description); items are ordered by `position` so the UI can re-order them.
-- Both tables are additive. `clip_collection_items` references `stream_clips`
-- via ON DELETE CASCADE so removing a clip automatically removes it from every
-- collection it belongs to.
CREATE TABLE clip_collections (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX clip_collections_creator_idx ON clip_collections(creator_id);

CREATE TABLE clip_collection_items (
    collection_id UUID NOT NULL REFERENCES clip_collections(id) ON DELETE CASCADE,
    clip_id UUID NOT NULL REFERENCES stream_clips(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (collection_id, clip_id)
);
CREATE INDEX clip_collection_items_collection_idx ON clip_collection_items(collection_id, position);
