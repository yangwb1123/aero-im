-- Durable per-room message delivery order.
--
-- Message ULIDs are time-sortable identifiers, not a contiguous delivery log:
-- concurrent writers (especially on different nodes) may commit/publish them in
-- an order that differs from UUID ordering.  A reconnect cursor therefore must
-- not treat MAX(message_id) as proof that every lower id was delivered.
--
-- `delivery_ordinal` is allocated while the message transaction holds the
-- room's counter row.  The matching creation outbox rows are claimed in this
-- order (see EventOutboxRepo), so an ACK of ordinal N is a true cumulative
-- prefix even when ULIDs or NATS publication attempts race.

ALTER TABLE messages
    ADD COLUMN IF NOT EXISTS delivery_ordinal BIGINT;

-- The first transactional execution ranks the complete room history. Migration
-- rollback prevents a normal deploy from leaving a partially assigned state;
-- re-execution updates NULL rows only, so a completed migration is a no-op and
-- already-certified ordinals are never rewritten or reordered.
WITH ranked AS (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY room_id
               ORDER BY created_at, id
           ) AS delivery_ordinal
      FROM messages
)
UPDATE messages AS message
   SET delivery_ordinal = ranked.delivery_ordinal
  FROM ranked
 WHERE message.id = ranked.id
   AND message.delivery_ordinal IS NULL;

CREATE TABLE IF NOT EXISTS room_delivery_sequences (
    room_id       UUID   PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
    last_ordinal  BIGINT NOT NULL CHECK (last_ordinal >= 0)
);

INSERT INTO room_delivery_sequences (room_id, last_ordinal)
SELECT room_id, COALESCE(MAX(delivery_ordinal), 0)
  FROM messages
 GROUP BY room_id
ON CONFLICT (room_id) DO UPDATE
      SET last_ordinal = GREATEST(
              room_delivery_sequences.last_ordinal,
              EXCLUDED.last_ordinal
          );

CREATE OR REPLACE FUNCTION assign_message_delivery_ordinal()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO room_delivery_sequences (room_id, last_ordinal)
         VALUES (NEW.room_id, 1)
    ON CONFLICT (room_id) DO UPDATE
            SET last_ordinal = room_delivery_sequences.last_ordinal + 1
      RETURNING last_ordinal INTO NEW.delivery_ordinal;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS messages_assign_delivery_ordinal ON messages;
CREATE TRIGGER messages_assign_delivery_ordinal
BEFORE INSERT ON messages
FOR EACH ROW
WHEN (NEW.delivery_ordinal IS NULL)
EXECUTE FUNCTION assign_message_delivery_ordinal();

ALTER TABLE messages
    DROP CONSTRAINT IF EXISTS messages_delivery_ordinal_positive;

ALTER TABLE messages
    ALTER COLUMN delivery_ordinal SET NOT NULL,
    ADD CONSTRAINT messages_delivery_ordinal_positive
        CHECK (delivery_ordinal > 0);

CREATE UNIQUE INDEX IF NOT EXISTS messages_room_delivery_ordinal_key
    ON messages (room_id, delivery_ordinal);

-- `messages_partitioned` is the additive cutover shadow created by migration
-- 0148. LIKE is a one-time schema snapshot, so every later live-message column
-- must be mirrored explicitly. Existing shadow rows may already have been
-- backfilled; reconcile them from the canonical live row before enforcing the
-- same NOT NULL/positive invariant.
ALTER TABLE messages_partitioned
    ADD COLUMN IF NOT EXISTS delivery_ordinal BIGINT;

UPDATE messages_partitioned AS shadow
   SET delivery_ordinal = live.delivery_ordinal
  FROM messages AS live
 WHERE shadow.id = live.id
   AND shadow.created_at = live.created_at
   AND shadow.delivery_ordinal IS DISTINCT FROM live.delivery_ordinal;

ALTER TABLE messages_partitioned
    DROP CONSTRAINT IF EXISTS messages_delivery_ordinal_positive;

ALTER TABLE messages_partitioned
    ALTER COLUMN delivery_ordinal SET NOT NULL,
    ADD CONSTRAINT messages_delivery_ordinal_positive
        CHECK (delivery_ordinal > 0);

-- PostgreSQL requires every UNIQUE index on a partitioned parent to contain
-- the partition key. The room counter remains the cross-partition source of
-- uniqueness; this index enforces the strongest declarative shape PG permits
-- and supports ordinal replay after the manual cutover.
CREATE UNIQUE INDEX IF NOT EXISTS messages_partitioned_room_delivery_ordinal_key
    ON messages_partitioned (room_id, delivery_ordinal, created_at);

-- Backfill supplies the live ordinal explicitly, so this trigger does not fire
-- during shadow copy. It is already attached when the shadow becomes `messages`
-- in the manual cutover, ensuring post-cutover inserts keep using the same
-- transactional room counter.
DROP TRIGGER IF EXISTS messages_assign_delivery_ordinal ON messages_partitioned;
CREATE TRIGGER messages_assign_delivery_ordinal
BEFORE INSERT ON messages_partitioned
FOR EACH ROW
WHEN (NEW.delivery_ordinal IS NULL)
EXECUTE FUNCTION assign_message_delivery_ordinal();

-- Reissue the shadow backfill projection with every non-generated live column,
-- including the MLS payload/version additions and this delivery ordinal.
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
            delivery_ordinal
        )
        SELECT
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text,
            mls_group_id, mls_epoch, mls_payload, expires_at, version,
            delivery_ordinal
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
    'id-ordered batches, including MLS, version, and delivery_ordinal (0174). '
    'Returns (rows_copied, last_id); 0 rows_copied = caught up.';

ALTER TABLE event_outbox
    ADD COLUMN IF NOT EXISTS delivery_ordinal BIGINT;

UPDATE event_outbox AS outbox
   SET delivery_ordinal = message.delivery_ordinal
  FROM messages AS message
 WHERE outbox.message_id = message.id
   AND outbox.delivery_ordinal IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS event_outbox_message_delivery_ordinal_key
    ON event_outbox (subject, delivery_ordinal)
    WHERE event_kind = 'message' AND delivery_ordinal IS NOT NULL;

ALTER TABLE event_outbox
    DROP CONSTRAINT IF EXISTS event_outbox_delivery_ordinal_positive;

ALTER TABLE event_outbox
    ADD CONSTRAINT event_outbox_delivery_ordinal_positive
        CHECK (delivery_ordinal IS NULL OR delivery_ordinal > 0);

CREATE INDEX IF NOT EXISTS idx_event_outbox_message_delivery_pending
    ON event_outbox (subject, delivery_ordinal)
    WHERE event_kind = 'message'
      AND delivery_ordinal IS NOT NULL
      AND published_at IS NULL;

ALTER TABLE delivery_cursors
    ADD COLUMN IF NOT EXISTS last_delivery_ordinal BIGINT;

-- Legacy cursors independently maximized a message ULID and bus seq. That old
-- state is not evidence of a contiguous content prefix, so never translate its
-- message id into an ordinal. Starting at zero deliberately causes one complete
-- ordered room replay; only a v2 application ACK may certify a newer prefix.
UPDATE delivery_cursors
   SET last_delivery_ordinal = 0
 WHERE last_delivery_ordinal IS NULL;

ALTER TABLE delivery_cursors
    DROP CONSTRAINT IF EXISTS delivery_cursors_ordinal_non_negative;

ALTER TABLE delivery_cursors
    ALTER COLUMN last_delivery_ordinal SET DEFAULT 0,
    ALTER COLUMN last_delivery_ordinal SET NOT NULL,
    ADD CONSTRAINT delivery_cursors_ordinal_non_negative
        CHECK (last_delivery_ordinal >= 0);

CREATE INDEX IF NOT EXISTS delivery_cursors_room_ordinal_idx
    ON delivery_cursors (room_id, last_delivery_ordinal);
