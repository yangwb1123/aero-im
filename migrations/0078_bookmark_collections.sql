-- 0078_bookmark_collections.sql — bookmark folders / collections
--
-- `bookmarks` (migration 0020) is a flat per-user saved-items list. This adds
-- the Slack/Lark "Saved items" folder affordance: a user groups their saved
-- messages into named, ordered collections. A collection is per-user; a saved
-- message belongs to at most ONE collection (a nullable `collection_id` on the
-- bookmark). Deleting a collection does NOT remove the bookmarks it held — it
-- nulls their `collection_id` (ON DELETE SET NULL), so the items fall back into
-- the un-foldered list rather than disappearing.

CREATE TABLE IF NOT EXISTS bookmark_collections (
    id             UUID        PRIMARY KEY,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    name           TEXT        NOT NULL,
    -- Manual sidebar ordering (ascending); ties broken by created_at.
    position       INTEGER     NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Listing is "this user's collections", ordered for the sidebar.
CREATE INDEX IF NOT EXISTS bookmark_collections_participant_idx
    ON bookmark_collections (participant_id, position ASC, created_at ASC);

-- A saved message's optional collection assignment. Nullable: an unassigned
-- bookmark sits in the flat list. ON DELETE SET NULL so removing a collection
-- keeps the underlying bookmarks (they just become un-foldered).
ALTER TABLE bookmarks
    ADD COLUMN IF NOT EXISTS collection_id UUID
        REFERENCES bookmark_collections(id) ON DELETE SET NULL;

-- "Saved items in this collection" filter for a given user.
CREATE INDEX IF NOT EXISTS bookmarks_collection_idx
    ON bookmarks (participant_id, collection_id);
