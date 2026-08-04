-- Commit-time authorization and cross-room containment for pinned messages.
--
-- Canonical application writes lock workspace -> room/member -> message -> pin.
-- The trigger deliberately uses lock-free current-access predicates so a raw
-- child-row write cannot invert that order by locking the pin/message first and
-- then waiting on workspace governance.

-- Remove only impossible cross-room projections before installing the composite
-- lifecycle edge. Same-room pins for soft-deleted messages remain removable by
-- the authorized unpin path.
DELETE FROM pins AS pin
 USING messages AS message
 WHERE message.id = pin.message_id
   AND pin.room_id <> message.room_id;

ALTER TABLE pins
    DROP CONSTRAINT IF EXISTS pins_message_id_fkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'pins_message_room_fkey'
           AND conrelid = 'pins'::regclass
    ) THEN
        ALTER TABLE pins
            ADD CONSTRAINT pins_message_room_fkey
            FOREIGN KEY (message_id, room_id)
            REFERENCES messages (id, room_id)
            ON DELETE CASCADE;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS pins_message_room_idx
    ON pins (message_id, room_id);

CREATE OR REPLACE FUNCTION pin_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_room uuid;
    canonical_workspace uuid;
    message_deleted_at timestamptz;
    message_expires_at timestamptz;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'pin aggregates are immutable after insertion'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'pins_identity_immutable_chk';
    END IF;

    SELECT message.room_id,
           room.workspace_id,
           message.deleted_at,
           message.expires_at
      INTO canonical_room,
           canonical_workspace,
           message_deleted_at,
           message_expires_at
      FROM messages AS message
      JOIN rooms AS room
        ON room.id = message.room_id
     WHERE message.id = NEW.message_id;

    IF NOT FOUND OR canonical_room <> NEW.room_id THEN
        RAISE EXCEPTION 'pin message must belong to the canonical room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'pins_message_room_scope_chk';
    END IF;

    IF message_deleted_at IS NOT NULL
       OR (
           message_expires_at IS NOT NULL
           AND message_expires_at <= CURRENT_TIMESTAMP
       ) THEN
        RAISE EXCEPTION 'only a live message may be pinned'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'pins_message_live_chk';
    END IF;

    IF NOT aero_participant_has_effective_workspace_access(
        canonical_workspace,
        NEW.pinned_by
    )
       OR NOT EXISTS (
           SELECT 1
             FROM room_members AS member
            WHERE member.room_id = NEW.room_id
              AND member.participant_id = NEW.pinned_by
       ) THEN
        RAISE EXCEPTION 'pin actor lacks current effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'pins_pinned_by_room_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS pin_transaction_fence
    ON pins;
CREATE TRIGGER pin_transaction_fence
    BEFORE INSERT OR UPDATE
    ON pins
    FOR EACH ROW
    EXECUTE FUNCTION pin_transaction_fence();

COMMENT ON TRIGGER pin_transaction_fence
    ON pins IS
    'Lock-free SQL backstop for canonical live-message containment, current pin actor access, and immutable pin aggregates';
