-- Personal channel-section mappings may be added only for channels in the
-- section's workspace that the section owner can access at the write boundary.
--
-- Historical rows predate room containment. Remove orphan, cross-workspace, and
-- non-channel mappings before adding the room FK. The FK uses ON DELETE CASCADE:
-- deleting a room cleans personal sidebar metadata instead of being blocked by
-- it.

DELETE FROM channel_section_items item
 USING channel_sections section
 WHERE item.section_id = section.id
   AND NOT EXISTS (
       SELECT 1
         FROM rooms room
        WHERE room.id = item.room_id
          AND room.workspace_id = section.workspace_id
          AND room.kind = 'channel'
   );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'channel_section_items_room_id_fkey'
           AND conrelid = 'channel_section_items'::regclass
    ) THEN
        ALTER TABLE channel_section_items
            ADD CONSTRAINT channel_section_items_room_id_fkey
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE
            NOT VALID;
    END IF;
END
$$;

ALTER TABLE channel_section_items
    VALIDATE CONSTRAINT channel_section_items_room_id_fkey;

CREATE INDEX IF NOT EXISTS channel_section_items_room_idx
    ON channel_section_items (room_id);

CREATE OR REPLACE FUNCTION aero_validate_channel_section_item_containment()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_workspace uuid;
    resolved_participant uuid;
    locked_workspace uuid;
    locked_participant uuid;
    room_workspace uuid;
    room_kind text;
BEGIN
    -- Resolve the authorization route without a section lock. The canonical
    -- effective-room helper acquires the workspace/access-edge/room fences; the
    -- section is locked and revalidated afterward.
    SELECT workspace_id, participant_id
      INTO resolved_workspace, resolved_participant
      FROM channel_sections
     WHERE id = NEW.section_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'channel section does not exist'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_section_items_room_containment';
    END IF;

    IF NOT aero_effective_room_access(
        NEW.room_id,
        resolved_participant,
        resolved_workspace
    ) THEN
        RAISE EXCEPTION 'section owner cannot access requested room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_section_items_room_containment';
    END IF;

    SELECT workspace_id, kind
      INTO room_workspace, room_kind
      FROM rooms
     WHERE id = NEW.room_id
       FOR SHARE;
    IF NOT FOUND
       OR room_workspace <> resolved_workspace
       OR room_kind <> 'channel' THEN
        RAISE EXCEPTION 'section item must reference a channel in its workspace'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_section_items_room_containment';
    END IF;

    SELECT workspace_id, participant_id
      INTO locked_workspace, locked_participant
      FROM channel_sections
     WHERE id = NEW.section_id
       FOR SHARE;
    IF NOT FOUND
       OR locked_workspace <> resolved_workspace
       OR locked_participant <> resolved_participant THEN
        RAISE EXCEPTION 'channel section authorization scope changed'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_section_items_room_containment';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS channel_section_items_containment
    ON channel_section_items;
CREATE TRIGGER channel_section_items_containment
    BEFORE INSERT OR UPDATE OF section_id, room_id
    ON channel_section_items
    FOR EACH ROW
    EXECUTE FUNCTION aero_validate_channel_section_item_containment();

COMMENT ON FUNCTION aero_validate_channel_section_item_containment() IS
    'Fences section owner, tenant, channel kind, and effective room access without blocking room deletes';
