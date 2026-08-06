-- Migration 0238: Message recall (撤回).
--
-- Recall replaces a message's content with a system placeholder while keeping
-- the row (message_id / room history / audit). Two nullable columns record the
-- recall; NULL recalled_at = not recalled. recalled_at and deleted_at may
-- coexist (a recalled message can still be tombstoned afterwards); the state
-- machine is enforced in code, not by a cross-column CHECK.
--
-- Expand–Migrate–Contract: purely additive, no data backfill, no dual-write.
-- The only CREATE OR REPLACE below reissues the partition-cutover prep
-- function (0174 convention) with the new columns — a projection fix, not a
-- data migration.

ALTER TABLE messages
    ADD COLUMN IF NOT EXISTS recalled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS recalled_by UUID REFERENCES participants(id);

-- `messages_partitioned` is the additive cutover shadow created by migration
-- 0148 (LIKE snapshot). Migration 0174 established the convention that every
-- later live-message column must be mirrored explicitly; reconcile existing
-- shadow rows from the canonical live row (NULL-safe, idempotent, re-runnable).
ALTER TABLE messages_partitioned
    ADD COLUMN IF NOT EXISTS recalled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS recalled_by UUID;

UPDATE messages_partitioned AS shadow
   SET recalled_at = live.recalled_at,
       recalled_by = live.recalled_by
  FROM messages AS live
 WHERE shadow.id = live.id
   AND shadow.created_at = live.created_at
   AND shadow.recalled_at IS DISTINCT FROM live.recalled_at;

-- event_outbox.event_kind CHECK extension (drop+add pattern from migration
-- 0211): the recall outbox event rides the same durable room-event queue.
ALTER TABLE event_outbox
    DROP CONSTRAINT IF EXISTS event_outbox_kind_check;

ALTER TABLE event_outbox
    ADD CONSTRAINT event_outbox_kind_check
        CHECK (
            event_kind IN (
                'message',
                'edited',
                'deleted',
                'notify',
                'reaction',
                'canvas_op',
                'recalled'
            )
        );

-- Change-replay must surface recalls to offline/reconnecting clients: recall
-- does NOT bump `edited_at`, so migration 0125's index on
-- GREATEST(edited_at, deleted_at) would leave recalled messages invisible to
-- `changes_since` (the reconnect mutation backfill) forever. Reissue the
-- expression index with recalled_at included. DROP+CREATE is safe here — the
-- index is a plain lookup accelerator, not a constraint, and the new
-- expression covers the old predicate's rows.
DROP INDEX IF EXISTS idx_messages_room_mutated;

CREATE INDEX IF NOT EXISTS idx_messages_room_mutated
    ON messages (room_id, GREATEST(edited_at, deleted_at, recalled_at));

-- Reissue the shadow backfill projection with the recall columns. Convention
-- (0148/0149/0158/0174): `backfill_messages_partition` lists source columns
-- explicitly, so every later live-message column must be added here or a
-- partition-cutover backfill would silently drop recall state in the shadow
-- (badge/guard/replay break at cutover).
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
            created_at, edited_at, deleted_at, searchable_text,
            mls_group_id, mls_epoch, mls_payload, expires_at, version,
            delivery_ordinal, recalled_at, recalled_by
        )
        SELECT
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text,
            mls_group_id, mls_epoch, mls_payload, expires_at, version,
            delivery_ordinal, recalled_at, recalled_by
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
    'ADDITIVE prep: incrementally copy messages -> messages_partitioned in '
    'id-ordered batches, including MLS, version, delivery_ordinal (0174), and '
    'recall columns (0238). Returns (rows_copied, last_id); 0 rows_copied = caught up.';
