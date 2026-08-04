-- Keep task assignment and source-message references inside the task's room.
--
-- Historical rows predate repository-level containment. Repair those rows
-- before adding ordinary referential constraints; current membership remains
-- an application invariant because PostgreSQL CHECK constraints cannot contain
-- the required room/workspace membership subquery.

UPDATE tasks task
   SET assignee_id = NULL,
       updated_at = now()
 WHERE assignee_id IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM room_members room_member
         JOIN rooms room
           ON room.id = room_member.room_id
         JOIN workspace_members workspace_member
           ON workspace_member.workspace_id = room.workspace_id
          AND workspace_member.participant_id = room_member.participant_id
         JOIN participants participant
           ON participant.id = room_member.participant_id
          AND participant.deleted_at IS NULL
        WHERE room_member.room_id = task.room_id
          AND room_member.participant_id = task.assignee_id
   );

UPDATE tasks task
   SET source_message_id = NULL,
       updated_at = now()
 WHERE source_message_id IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM messages message
        WHERE message.id = task.source_message_id
          AND message.room_id = task.room_id
   );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'tasks_room_id_fkey'
           AND conrelid = 'tasks'::regclass
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_room_id_fkey
            FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'tasks_creator_id_fkey'
           AND conrelid = 'tasks'::regclass
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_creator_id_fkey
            FOREIGN KEY (creator_id) REFERENCES participants(id)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'tasks_assignee_id_fkey'
           AND conrelid = 'tasks'::regclass
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_assignee_id_fkey
            FOREIGN KEY (assignee_id) REFERENCES participants(id)
            ON DELETE SET NULL
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'tasks_source_message_id_fkey'
           AND conrelid = 'tasks'::regclass
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_source_message_id_fkey
            FOREIGN KEY (source_message_id) REFERENCES messages(id)
            ON DELETE SET NULL
            NOT VALID;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS tasks_assignee_room_idx
    ON tasks (assignee_id, room_id)
    WHERE assignee_id IS NOT NULL;
