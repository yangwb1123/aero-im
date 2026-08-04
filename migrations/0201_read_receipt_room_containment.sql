-- A read cursor is meaningful only inside the room named by the receipt.
-- Historically `last_read_message_id` had no foreign key or trigger, so a
-- client that knew a message UUID from another room could persist that UUID as
-- this room's cursor. Remove only proven cross-room pollution; a missing legacy
-- anchor is retained because ephemeral-message hard deletion intentionally does
-- not reset an otherwise useful monotonic cursor.
DELETE FROM read_receipts receipt
 WHERE EXISTS (
           SELECT 1
             FROM messages message
            WHERE message.id = receipt.last_read_message_id
       )
   AND NOT EXISTS (
           SELECT 1
             FROM messages message
            WHERE message.id = receipt.last_read_message_id
              AND message.room_id = receipt.room_id
       );

CREATE OR REPLACE FUNCTION read_receipt_message_room_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Lock the immutable message identity through the receipt write. A trigger
    -- is used instead of an FK so later hard deletion of an ephemeral anchor
    -- does not cascade-delete the participant's entire room cursor.
    PERFORM 1
      FROM messages message
     WHERE message.id = NEW.last_read_message_id
       AND message.room_id = NEW.room_id
       FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'read receipt message % does not belong to room %',
            NEW.last_read_message_id,
            NEW.room_id
            USING ERRCODE = '23514',
                  CONSTRAINT = 'read_receipts_message_room_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS read_receipt_message_room_guard
    ON read_receipts;
CREATE TRIGGER read_receipt_message_room_guard
    BEFORE INSERT OR UPDATE OF room_id, last_read_message_id
    ON read_receipts
    FOR EACH ROW
    EXECUTE FUNCTION read_receipt_message_room_guard();

COMMENT ON TRIGGER read_receipt_message_room_guard ON read_receipts IS
    'Rejects cross-room or nonexistent message cursors while preserving legacy cursors after later ephemeral hard deletion.';
