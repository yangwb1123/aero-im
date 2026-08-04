-- A reply is contained by the room of its parent message.
--
-- The original single-column FK only proved that `reply_to` existed. A guessed
-- message id from another room could therefore create a cross-room thread edge
-- and make thread reads disclose that reply. Clean historical edges before
-- replacing the FK, then enforce the invariant for every database writer.

LOCK TABLE messages IN SHARE ROW EXCLUSIVE MODE;

UPDATE messages AS reply
   SET reply_to = NULL
 WHERE reply.reply_to IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM messages AS parent
        WHERE parent.id = reply.reply_to
          AND parent.room_id = reply.room_id
   );

ALTER TABLE messages
    DROP CONSTRAINT IF EXISTS messages_reply_to_fkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'messages'::regclass
           AND conname = 'messages_id_room_id_key'
    ) THEN
        ALTER TABLE messages
            ADD CONSTRAINT messages_id_room_id_key UNIQUE (id, room_id);
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'messages'::regclass
           AND conname = 'messages_reply_same_room_fkey'
    ) THEN
        ALTER TABLE messages
            ADD CONSTRAINT messages_reply_same_room_fkey
            FOREIGN KEY (reply_to, room_id)
            REFERENCES messages (id, room_id)
            ON DELETE SET NULL (reply_to)
            DEFERRABLE INITIALLY IMMEDIATE;
    END IF;
END
$$;

COMMENT ON CONSTRAINT messages_reply_same_room_fkey ON messages IS
    'reply_to must reference a message in the same room; hard-deleting the parent clears reply_to only';
