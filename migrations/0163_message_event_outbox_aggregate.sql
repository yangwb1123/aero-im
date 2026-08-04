-- 0163_message_event_outbox_aggregate.sql
--
-- A message is an event aggregate, not a one-shot producer.  Creation, edits,
-- system patches, and deletion each append a distinct event while preserving a
-- strict per-message publication order.  The relay only claims the earliest
-- unpublished aggregate version, so a later edit/delete can never overtake a
-- failed create publish.

ALTER TABLE event_outbox
    DROP CONSTRAINT IF EXISTS event_outbox_message_id_key;

ALTER TABLE event_outbox
    ADD COLUMN IF NOT EXISTS event_kind TEXT,
    ADD COLUMN IF NOT EXISTS aggregate_version BIGINT;

-- Backfill the one create event produced by migration 0162.  Be defensive
-- about hand-written payloads so an upgrade never leaves the new columns NULL.
UPDATE event_outbox
   SET event_kind = CASE payload->>'kind'
                        WHEN 'edited' THEN 'edited'
                        WHEN 'deleted' THEN 'deleted'
                        WHEN 'notify_batch' THEN 'notify'
                        ELSE 'message'
                    END
 WHERE event_kind IS NULL;

WITH ranked AS (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY message_id
               ORDER BY created_at, id
           ) AS aggregate_version
      FROM event_outbox
     WHERE aggregate_version IS NULL
)
UPDATE event_outbox AS outbox
   SET aggregate_version = ranked.aggregate_version
  FROM ranked
 WHERE outbox.id = ranked.id;

ALTER TABLE event_outbox
    ALTER COLUMN event_kind SET NOT NULL,
    ALTER COLUMN aggregate_version SET NOT NULL,
    ADD CONSTRAINT event_outbox_kind_check
        CHECK (event_kind IN ('message', 'edited', 'deleted', 'notify')),
    ADD CONSTRAINT event_outbox_aggregate_version_check
        CHECK (aggregate_version > 0),
    ADD CONSTRAINT event_outbox_message_version_key
        UNIQUE (message_id, aggregate_version);

CREATE INDEX IF NOT EXISTS idx_event_outbox_aggregate_pending
    ON event_outbox (message_id, aggregate_version)
    WHERE published_at IS NULL;
