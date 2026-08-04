-- Scheduled sends and private composer drafts retain a reply target before a
-- real message exists. Apply the same room-containment invariant as messages so
-- invalid payloads cannot be accepted now and fail only at delivery time.

LOCK TABLE scheduled_messages IN SHARE ROW EXCLUSIVE MODE;
LOCK TABLE message_drafts IN SHARE ROW EXCLUSIVE MODE;

-- Migration cleanup is the only direct payload rewrite permitted after a
-- delivery attempt. The table lock above excludes concurrent writers, and a
-- migration failure rolls the trigger state back with the transaction.
ALTER TABLE scheduled_messages
    DISABLE TRIGGER scheduled_messages_delivery_fence;

UPDATE scheduled_messages AS scheduled
   SET reply_to = NULL
 WHERE scheduled.reply_to IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM messages AS parent
        WHERE parent.id = scheduled.reply_to
          AND parent.room_id = scheduled.room_id
   );

ALTER TABLE scheduled_messages
    ENABLE TRIGGER scheduled_messages_delivery_fence;

UPDATE message_drafts AS draft
   SET reply_to = NULL
 WHERE draft.reply_to IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM messages AS parent
        WHERE parent.id = draft.reply_to
          AND parent.room_id = draft.room_id
   );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'scheduled_messages'::regclass
           AND conname = 'scheduled_messages_reply_same_room_fkey'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_reply_same_room_fkey
            FOREIGN KEY (reply_to, room_id)
            REFERENCES messages (id, room_id)
            ON DELETE SET NULL (reply_to)
            DEFERRABLE INITIALLY IMMEDIATE;
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'message_drafts'::regclass
           AND conname = 'message_drafts_reply_same_room_fkey'
    ) THEN
        ALTER TABLE message_drafts
            ADD CONSTRAINT message_drafts_reply_same_room_fkey
            FOREIGN KEY (reply_to, room_id)
            REFERENCES messages (id, room_id)
            ON DELETE SET NULL (reply_to)
            DEFERRABLE INITIALLY IMMEDIATE;
    END IF;
END
$$;

-- Keep attempted scheduled payloads immutable for direct writers while
-- allowing the referential action above to clear a parent that is no longer
-- visible in the same room. A direct writer cannot clear a still-valid parent,
-- regardless of trigger nesting; all other payload fields remain fenced
-- exactly as in 0184.
CREATE OR REPLACE FUNCTION scheduled_messages_guard_delivery_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.canceled_at IS NULL
       AND NEW.canceled_at IS NOT NULL
       AND NEW.delivery_status = 'pending'
       AND NEW.attempts = 0 THEN
        NEW.delivery_status := 'canceled';
    END IF;

    IF NEW.delivery_generation < OLD.delivery_generation THEN
        RAISE EXCEPTION 'scheduled message delivery generation cannot move backwards'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.delivery_generation <> OLD.delivery_generation AND NOT (
        OLD.delivery_status = 'dead'
        AND NEW.delivery_status = 'pending'
        AND NEW.delivery_generation = OLD.delivery_generation + 1
        AND NEW.attempts = 0
    ) THEN
        RAISE EXCEPTION 'scheduled message delivery generation may only advance on dead retry'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.attempts < OLD.attempts AND NOT (
        OLD.delivery_status = 'dead'
        AND NEW.delivery_status = 'pending'
        AND NEW.delivery_generation = OLD.delivery_generation + 1
        AND NEW.attempts = 0
    ) THEN
        RAISE EXCEPTION 'scheduled message attempts cannot move backwards'
            USING ERRCODE = '23514';
    END IF;

    IF (OLD.attempts > 0 OR NEW.attempts > 0) AND (
        NEW.room_id IS DISTINCT FROM OLD.room_id
        OR NEW.sender_id IS DISTINCT FROM OLD.sender_id
        OR NEW.blocks IS DISTINCT FROM OLD.blocks
        OR (
            NEW.reply_to IS DISTINCT FROM OLD.reply_to
            AND NOT (
                OLD.reply_to IS NOT NULL
                AND NEW.reply_to IS NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM messages AS parent
                     WHERE parent.id = OLD.reply_to
                       AND parent.room_id = OLD.room_id
                )
            )
        )
        OR NEW.scheduled_at IS DISTINCT FROM OLD.scheduled_at
    ) THEN
        RAISE EXCEPTION 'scheduled message payload is immutable after delivery starts'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

COMMENT ON CONSTRAINT scheduled_messages_reply_same_room_fkey
    ON scheduled_messages IS
    'scheduled reply_to must reference a message in scheduled_messages.room_id';

COMMENT ON CONSTRAINT message_drafts_reply_same_room_fkey
    ON message_drafts IS
    'draft reply_to must reference a message in message_drafts.room_id';
