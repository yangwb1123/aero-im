-- 0158_messages_partition_shadow_version_column.sql
-- ===========================================================================
-- Closes a schema-drift gap between the `messages` table and its partitioning
-- shadow (`messages_partitioned`, migration 0148): migration 0157 added a
-- `version` column to `messages` for optimistic-locking (see
-- migrations/0157_message_version.sql) AFTER 0148 ran, so `messages_partitioned`
-- — built via `LIKE messages INCLUDING DEFAULTS` back when 0148 executed — never
-- picked it up. `LIKE` is a one-time snapshot at CREATE TIME, not a live mirror,
-- so any column added to `messages` after 0148 needs an explicit follow-up here.
--
-- Left unfixed, a cutover (docs/runbooks/messages-partitioning.md Step C) would
-- either fail outright (INSERT referencing a column messages_partitioned lacks)
-- or, if the shadow's own column list were patched without this one, every
-- migrated row would silently fall back to the column DEFAULT (version = 1),
-- discarding real edit-version history and causing every previously-edited
-- message's next edit to spuriously 409 post-cutover (the client's remembered
-- `expected_version` would permanently disagree with the reset-to-1 value).
-- Caught during a rescan of this runbook's own June verification record, before
-- any real cutover was attempted (docs/runbooks/messages-cutover.sql updated
-- alongside this migration; see its own change note + the fixed and re-run
-- verification in Runbook §5a).
--
-- ADDITIVE + IDEMPOTENT: guarded with IF NOT EXISTS, safe to re-run. Does NOT
-- touch the live `messages` table (only its as-yet-unused shadow), so — like
-- 0148 — this ships in the normal auto-applied chain.

ALTER TABLE messages_partitioned
    ADD COLUMN IF NOT EXISTS version INTEGER NOT NULL DEFAULT 1;

-- `backfill_messages_partition` (0148) must carry the real version, not let
-- every backfilled row silently take the column DEFAULT. Identical to the 0148
-- body in every other respect (same batching, same actual-inserted-count
-- semantics, same UUID-max-avoidance via `ORDER BY id DESC LIMIT 1`) — only
-- `version` is added to the INSERT's explicit column list (the `batch` CTE
-- already carries it via `m.*`).
CREATE OR REPLACE FUNCTION backfill_messages_partition(
    batch_size INT  DEFAULT 5000,
    from_id    UUID DEFAULT '00000000-0000-0000-0000-000000000000'
) RETURNS TABLE (rows_copied BIGINT, last_id UUID)
LANGUAGE plpgsql
AS $fn$
BEGIN
    RETURN QUERY
    WITH batch AS (
        SELECT m.*
          FROM messages m
         WHERE m.id > from_id
         ORDER BY m.id
         LIMIT batch_size
    ),
    ins AS (
        INSERT INTO messages_partitioned (
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at, version
        )
        SELECT
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at, version
          FROM batch
        ON CONFLICT DO NOTHING
        RETURNING id
    )
    SELECT
        (SELECT count(*) FROM ins)::BIGINT,
        COALESCE((SELECT id FROM batch ORDER BY id DESC LIMIT 1), from_id);
END
$fn$;

COMMENT ON FUNCTION backfill_messages_partition(INT, UUID) IS
    'ADDITIVE prep: incrementally copy messages -> messages_partitioned in id-ordered '
    'batches (ON CONFLICT DO NOTHING), including version (0158). Call in a throttled '
    'loop before the cutover window; returns (rows_copied, last_id). 0 rows_copied = '
    'caught up. Reads messages only -- never writes/locks the live table. See '
    'docs/runbooks/messages-partitioning.md.';
