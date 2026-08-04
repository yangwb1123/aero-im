-- Persist group-DM identity independently from mutable presentation metadata.
--
-- Historically a group DM was inferred from `kind = 'group' AND name IS NULL`.
-- Naming such a conversation therefore made it disappear from exact-set lookup
-- and listing, and a later open created a duplicate room.  Keep the product
-- identity in a dedicated persistent discriminator instead.

ALTER TABLE rooms
    ADD COLUMN IF NOT EXISTS is_group_dm BOOLEAN NOT NULL DEFAULT false;

-- Preserve the historical classification for existing small, nameless group
-- conversations.  The member-count bound mirrors the HTTP contract (3..=8).
UPDATE rooms AS room
   SET is_group_dm = true
 WHERE room.kind = 'group'
   AND room.name IS NULL
   AND (
       SELECT count(*)
         FROM room_members AS member
        WHERE member.room_id = room.id
   ) BETWEEN 3 AND 8;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'rooms'::regclass
           AND conname = 'rooms_group_dm_kind_check'
    ) THEN
        ALTER TABLE rooms
            ADD CONSTRAINT rooms_group_dm_kind_check
            CHECK (NOT is_group_dm OR kind = 'group')
            NOT VALID;
    END IF;
END
$$;

ALTER TABLE rooms
    VALIDATE CONSTRAINT rooms_group_dm_kind_check;

CREATE INDEX IF NOT EXISTS rooms_group_dm_workspace_created_idx
    ON rooms (workspace_id, created_at DESC)
    WHERE is_group_dm;
