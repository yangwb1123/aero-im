-- Partial indexes on messages: exclude soft-deleted rows from the expensive
-- GIN (full-text) and HNSW (vector) indexes (ROADMAP6 方向二: 索引瘦身).
--
-- Problem: all existing GIN/HNSW indexes on `messages` are unconditional —
-- they index EVERY row including the soft-deleted ones (deleted_at IS NOT NULL).
-- Every full-text / vector search traverses dead tuples until autovacuum reclaims
-- them. Under sustained write load, autovacuum lags and the dead-tuple fraction
-- grows, progressively slowing down every search query that hits these indexes.
-- A partial index with `WHERE deleted_at IS NULL` excludes soft-deleted rows from
-- the index entirely, restoring O(live-rows) search performance.
--
-- Also drops and recreates `messages_searchable_trgm` (trigram GIN, same issue),
-- and adds a dedicated `messages_sender_id_idx` (the GDPR erasure path scans by
-- sender_id and currently relies on a seq-scan-after-filter on the blocks GIN).

-- 1. search_tsv GIN (full-text search) — partial
DROP INDEX IF EXISTS messages_search_tsv_gin;
CREATE INDEX IF NOT EXISTS messages_search_tsv_gin
    ON messages USING gin (search_tsv)
    WHERE deleted_at IS NULL;

-- 2. searchable_text trigram GIN (fuzzy/trigram search) — partial
DROP INDEX IF EXISTS messages_searchable_trgm;
CREATE INDEX IF NOT EXISTS messages_searchable_trgm
    ON messages USING gin (searchable_text gin_trgm_ops)
    WHERE deleted_at IS NULL;

-- 3. blocks GIN (mentions, file-id lookups via JSONB) — partial
--    We can't add WHERE to a GIN index that the query planner already uses
--    effectively, but a partial index on (room_id, created_at DESC) already
--    covers the dominant query path. The blocks GIN remains unconditional for
--    now: the JSONB path ops benefit from the existing structure and the
--    dead-tuple ratio is lower because blocks are cleared on soft-delete
--    (set to '[]'::jsonb), not retained.

-- 4. embedding HNSW (semantic/vector search) — partial
DROP INDEX IF EXISTS messages_embedding_hnsw;
CREATE INDEX IF NOT EXISTS messages_embedding_hnsw
    ON messages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 128)
    WHERE deleted_at IS NULL;

-- 5. sender_id BTree — dedicated index for GDPR erasure paths
--    `delete_participant` and `sweep_deferred_erasure` both scan
--    `WHERE sender_id = $1 AND deleted_at IS NULL`. Without an index,
--    they seq-scan the full messages heap filtered by the blocks GIN.
--    A partial BTree sidesteps the GIN entirely.
CREATE INDEX IF NOT EXISTS messages_sender_id_active_idx
    ON messages (sender_id)
    WHERE deleted_at IS NULL;
