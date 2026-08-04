-- Reaction tenant/message containment and durable-event support.
--
-- Canonical toggles lock effective room access and the live message, mutate the
-- reaction projection, and append a `reaction` message-aggregate outbox row in
-- one transaction.  The guards below backstop raw SQL: a new reaction cannot be
-- forged by a participant outside the message room, reaction identity cannot be
-- rewritten, and a tombstoned message cannot retain or gain visible reactions.

-- Historical rows outside the long-standing API byte limit are not renderable
-- by supported clients.  Reactions are a visible projection, not audit evidence,
-- so removing invalid rows is preferable to blocking this invariant upgrade.
DELETE FROM reactions
 WHERE octet_length(emoji) NOT BETWEEN 1 AND 32;

-- Canonical soft deletion already removes this projection in MessageRepo.  Clean
-- any rows left by older/raw writers before installing the tombstone trigger.
DELETE FROM reactions AS reaction
 USING messages AS message
 WHERE message.id = reaction.message_id
   AND message.deleted_at IS NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'reactions_emoji_bytes_chk'
           AND conrelid = 'reactions'::regclass
    ) THEN
        ALTER TABLE reactions
            ADD CONSTRAINT reactions_emoji_bytes_chk
            CHECK (octet_length(emoji) BETWEEN 1 AND 32);
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION reaction_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_room uuid;
    locked_room uuid;
    message_deleted_at timestamptz;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.message_id IS DISTINCT FROM OLD.message_id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.emoji IS DISTINCT FROM OLD.emoji
       ) THEN
        RAISE EXCEPTION 'reaction aggregate identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'reactions_identity_immutable_chk';
    END IF;

    -- Resolve without locking, then let the canonical helper acquire
    -- workspace -> room -> membership locks.  The message lock comes last.
    SELECT room_id
      INTO resolved_room
      FROM messages
     WHERE id = NEW.message_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'reaction message does not exist'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'reactions_message_scope_chk';
    END IF;

    IF NOT aero_effective_room_access(
        resolved_room,
        NEW.participant_id,
        NULL
    ) THEN
        RAISE EXCEPTION 'reaction participant lacks effective message-room access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'reactions_participant_scope_chk';
    END IF;

    SELECT room_id, deleted_at
      INTO locked_room, message_deleted_at
      FROM messages
     WHERE id = NEW.message_id
       FOR SHARE;
    IF NOT FOUND
       OR locked_room <> resolved_room
       OR message_deleted_at IS NOT NULL THEN
        RAISE EXCEPTION 'reaction message is missing, moved, or deleted'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'reactions_message_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS reaction_scope_guard
    ON reactions;
CREATE TRIGGER reaction_scope_guard
    BEFORE INSERT OR UPDATE
    ON reactions
    FOR EACH ROW
    EXECUTE FUNCTION reaction_scope_guard();

COMMENT ON TRIGGER reaction_scope_guard ON reactions IS
    'Backstops reaction identity, effective room membership, and live-message containment; canonical writes additionally append an outbox row in the same transaction.';

CREATE OR REPLACE FUNCTION purge_reactions_on_message_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    DELETE FROM reactions
     WHERE message_id = NEW.id;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS purge_reactions_on_message_tombstone
    ON messages;
CREATE TRIGGER purge_reactions_on_message_tombstone
    AFTER UPDATE OF deleted_at
    ON messages
    FOR EACH ROW
    WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION purge_reactions_on_message_tombstone();

COMMENT ON TRIGGER purge_reactions_on_message_tombstone ON messages IS
    'Keeps reactions a live-message-only projection even for raw SQL tombstones; hard deletes remain covered by the reactions message FK cascade.';

-- Reactions join the same per-message aggregate outbox used by create/edit/
-- delete/notify so a committed toggle cannot be lost between PostgreSQL and
-- NATS, and cannot overtake an earlier message mutation.
ALTER TABLE event_outbox
    DROP CONSTRAINT IF EXISTS event_outbox_kind_check;

ALTER TABLE event_outbox
    ADD CONSTRAINT event_outbox_kind_check
        CHECK (event_kind IN ('message', 'edited', 'deleted', 'notify', 'reaction'));

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'event_outbox_reaction_payload_chk'
           AND conrelid = 'event_outbox'::regclass
    ) THEN
        ALTER TABLE event_outbox
            ADD CONSTRAINT event_outbox_reaction_payload_chk
            CHECK (
                event_kind <> 'reaction'
                OR COALESCE((
                    jsonb_typeof(payload->'kind') = 'string'
                    AND jsonb_typeof(payload->'room_id') = 'string'
                    AND jsonb_typeof(payload->'message_id') = 'string'
                    AND jsonb_typeof(payload->'participant') = 'string'
                    AND jsonb_typeof(payload->'emoji') = 'string'
                    AND jsonb_typeof(payload->'op') = 'string'
                    AND payload->>'kind' = 'reaction'
                    AND payload->>'message_id' = aero_uuid_to_ulid(message_id)
                    AND subject = 'im.room.' || (payload->>'room_id')
                    AND octet_length(payload->>'emoji') BETWEEN 1 AND 32
                    AND payload->>'op' IN ('add', 'remove')
                ), false)
            );
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION reaction_outbox_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_room uuid;
    locked_room uuid;
    event_participant uuid;
    message_deleted_at timestamptz;
BEGIN
    IF NEW.event_kind <> 'reaction' THEN
        RETURN NEW;
    END IF;

    -- Resolve identities without locks first, then acquire the canonical
    -- workspace -> room -> membership fence before locking the message last.
    SELECT room_id
      INTO resolved_room
      FROM messages
     WHERE id = NEW.message_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'reaction outbox message/room identity mismatch'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'event_outbox_reaction_scope_chk';
    END IF;

    SELECT membership.participant_id
      INTO event_participant
      FROM room_members AS membership
     WHERE membership.room_id = resolved_room
       AND aero_uuid_to_ulid(membership.participant_id) =
           NEW.payload->>'participant';
    IF NOT FOUND
       OR NOT aero_effective_room_access(
           resolved_room,
           event_participant,
           NULL
       ) THEN
        RAISE EXCEPTION 'reaction outbox participant lacks effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'event_outbox_reaction_participant_scope_chk';
    END IF;

    SELECT room_id, deleted_at
      INTO locked_room, message_deleted_at
      FROM messages
     WHERE id = NEW.message_id
       FOR KEY SHARE;
    IF NOT FOUND
       OR locked_room <> resolved_room
       OR message_deleted_at IS NOT NULL
       OR aero_uuid_to_ulid(locked_room) IS DISTINCT FROM
          NEW.payload->>'room_id' THEN
        RAISE EXCEPTION 'reaction outbox message is missing, moved, or deleted'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'event_outbox_reaction_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS reaction_outbox_scope_guard
    ON event_outbox;
CREATE TRIGGER reaction_outbox_scope_guard
    BEFORE INSERT OR UPDATE OF message_id, event_kind, subject, payload
    ON event_outbox
    FOR EACH ROW
    EXECUTE FUNCTION reaction_outbox_scope_guard();

COMMENT ON TRIGGER reaction_outbox_scope_guard ON event_outbox IS
    'Backstops reaction event message/room/participant containment and live-message scope; ReactionRepo owns atomic projection + outbox creation.';
