-- Index tuning for the data plane (ROADMAP 方向四).
--
-- Two changes, both idempotent (IF EXISTS / IF NOT EXISTS) so the migration is
-- safe to re-run and safe on a fresh DB where the embedding column has no rows
-- yet:
--   1. Rebuild the HNSW vector index with explicit, higher-recall build params.
--   2. Add a covering composite index for the unread-count hot path.

-- 1. HNSW vector index ------------------------------------------------------
--
-- 0001_init created `messages_embedding_hnsw` with pgvector's DEFAULT build
-- params (m = 16, ef_construction = 64). The stock build-time ef of 64 trades
-- recall for build speed; for a semantic-search index that is queried far more
-- often than it is rebuilt, a higher ef_construction is the right trade.
--
-- m = 16 (max edges per node — pgvector default; good for 1024-dim vectors)
-- ef_construction = 128 (candidate list size at build time — doubles pgvector's
--   stock 64, giving a denser, higher-recall graph at the cost of a slower one-
--   time build). 16/128 are solid production defaults for cosine search.
--
-- DROP + CREATE rather than ALTER because HNSW build params are fixed at index
-- creation and can only be changed by rebuilding.
DROP INDEX IF EXISTS messages_embedding_hnsw;
CREATE INDEX IF NOT EXISTS messages_embedding_hnsw
    ON messages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 128);

-- 2. Unread-count hot-path composite index ----------------------------------
--
-- Backs MessageRepo::unread_counts_by_room, whose plan is dominated by:
--     FROM messages m
--     JOIN room_members rm ON rm.room_id = m.room_id AND rm.participant_id = $1
--     LEFT JOIN read_receipts rr ON ... = m.room_id ...
--     WHERE m.deleted_at IS NULL
--       AND m.sender_id <> $1
--       AND (rr.last_read_message_id IS NULL OR m.id > rr.last_read_message_id)
--     GROUP BY m.room_id
--
-- The existing `messages_room_created_idx (room_id, created_at DESC)` does NOT
-- cover this: the unread predicate ranges on `m.id` (> last_read_message_id),
-- not created_at, and it filters out soft-deleted rows. A partial index keyed
-- on (room_id, id) lets Postgres satisfy the per-room scan and the `m.id >`
-- range with a single index-only-ish range scan over live rows, and feeds the
-- GROUP BY room_id directly (room_id is the leading column).
--
-- Partial on `deleted_at IS NULL` keeps the index small (tombstoned messages
-- are never unread) and matches the query's WHERE clause exactly.
CREATE INDEX IF NOT EXISTS messages_room_active_id
    ON messages (room_id, id)
    WHERE deleted_at IS NULL;
