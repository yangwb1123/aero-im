-- Atomic, replayable persistence for AI-extracted action-item batches.
--
-- Ordinary task creation remains unchanged: both new task columns default to
-- NULL and every batch-only constraint is conditional on a non-NULL key.  A
-- durable receipt is necessary even for an empty extraction, and retains the
-- original ordered task ids after individual tasks are later deleted.  Both
-- key columns store a lowercase SHA-256 digest, never the caller's raw token.

CREATE TABLE IF NOT EXISTS task_action_item_batches (
    participant_id  UUID        NOT NULL
                                REFERENCES participants(id) ON DELETE CASCADE,
    room_id         UUID        NOT NULL
                                REFERENCES rooms(id) ON DELETE CASCADE,
    idempotency_key TEXT        NOT NULL,
    task_ids        UUID[]      NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, room_id, idempotency_key),
    CONSTRAINT task_action_item_batches_key_shape_chk
        CHECK (
            octet_length(idempotency_key) = 64
            AND idempotency_key ~ '^[0-9a-f]{64}$'
        ),
    CONSTRAINT task_action_item_batches_size_chk
        CHECK (cardinality(task_ids) BETWEEN 0 AND 20)
);

ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS action_item_batch_key TEXT,
    ADD COLUMN IF NOT EXISTS action_item_batch_index SMALLINT;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'tasks'::regclass
           AND conname = 'tasks_action_item_batch_identity_chk'
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_action_item_batch_identity_chk
            CHECK (
                (
                    action_item_batch_key IS NULL
                    AND action_item_batch_index IS NULL
                )
                OR (
                    action_item_batch_key IS NOT NULL
                    AND octet_length(action_item_batch_key) = 64
                    AND action_item_batch_key ~ '^[0-9a-f]{64}$'
                    AND action_item_batch_index BETWEEN 0 AND 19
                )
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'tasks'::regclass
           AND conname = 'tasks_action_item_batch_title_chk'
    ) THEN
        -- The edge clamps AI titles to 512 Unicode scalar values. Keep this
        -- conditional so legacy/manual non-batch tasks retain old behavior.
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_action_item_batch_title_chk
            CHECK (
                action_item_batch_key IS NULL
                OR char_length(title) BETWEEN 1 AND 512
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'tasks'::regclass
           AND conname = 'tasks_action_item_batch_fkey'
    ) THEN
        ALTER TABLE tasks
            ADD CONSTRAINT tasks_action_item_batch_fkey
            FOREIGN KEY (creator_id, room_id, action_item_batch_key)
            REFERENCES task_action_item_batches
                (participant_id, room_id, idempotency_key)
            ON DELETE CASCADE
            NOT VALID;
    END IF;
END
$$;

CREATE UNIQUE INDEX IF NOT EXISTS tasks_action_item_batch_position_uq
    ON tasks (
        creator_id,
        room_id,
        action_item_batch_key,
        action_item_batch_index
    )
    WHERE action_item_batch_key IS NOT NULL;

CREATE OR REPLACE FUNCTION task_action_item_batch_receipt_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key
           OR NEW.task_ids IS DISTINCT FROM OLD.task_ids
           OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
            RAISE EXCEPTION 'action-item batch identity is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'task_action_item_batch_identity_immutable_chk';
        END IF;
        RETURN NEW;
    END IF;

    -- The receipt clock is security/idempotency metadata, not caller input.
    NEW.created_at := clock_timestamp();

    IF NOT aero_effective_room_access(
        NEW.room_id,
        NEW.participant_id,
        NULL
    ) THEN
        RAISE EXCEPTION
            'action-item batch creator requires current effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'task_action_item_batch_effective_access_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS task_action_item_batch_receipt_guard
    ON task_action_item_batches;
CREATE TRIGGER task_action_item_batch_receipt_guard
    BEFORE INSERT OR UPDATE OF
        participant_id,
        room_id,
        idempotency_key,
        task_ids,
        created_at
    ON task_action_item_batches
    FOR EACH ROW
    EXECUTE FUNCTION task_action_item_batch_receipt_guard();

CREATE OR REPLACE FUNCTION task_action_item_batch_task_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    expected_task_ids uuid[];
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           OLD.action_item_batch_key IS NOT NULL
           OR NEW.action_item_batch_key IS NOT NULL
       )
       AND (
           NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.creator_id IS DISTINCT FROM OLD.creator_id
           OR NEW.action_item_batch_key IS DISTINCT FROM
              OLD.action_item_batch_key
           OR NEW.action_item_batch_index IS DISTINCT FROM
              OLD.action_item_batch_index
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
       ) THEN
        RAISE EXCEPTION 'action-item task batch identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tasks_action_item_batch_identity_immutable_chk';
    END IF;

    IF NEW.action_item_batch_key IS NULL THEN
        RETURN NEW;
    END IF;

    -- A durable receipt intentionally survives individual task deletion, but it
    -- must not become an authorization capability after room access is revoked.
    -- Run the canonical workspace -> room -> membership fence before locking the
    -- receipt so a raw INSERT cannot resurrect a deleted batch child.
    IF TG_OP = 'INSERT'
       AND NOT aero_effective_room_access(
           NEW.room_id,
           NEW.creator_id,
           NULL
       ) THEN
        RAISE EXCEPTION
            'action-item task creator requires current effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tasks_action_item_batch_effective_access_chk';
    END IF;

    SELECT batch.task_ids
      INTO expected_task_ids
      FROM task_action_item_batches AS batch
     WHERE batch.participant_id = NEW.creator_id
       AND batch.room_id = NEW.room_id
       AND batch.idempotency_key = NEW.action_item_batch_key
       FOR KEY SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'action-item task requires its canonical batch receipt'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tasks_action_item_batch_scope_chk';
    END IF;

    IF NEW.action_item_batch_index < 0
       OR NEW.action_item_batch_index >= cardinality(expected_task_ids)
       OR expected_task_ids[NEW.action_item_batch_index + 1]
          IS DISTINCT FROM NEW.id THEN
        RAISE EXCEPTION
            'action-item task id/index does not match its batch receipt'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tasks_action_item_batch_position_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS task_action_item_batch_task_guard
    ON tasks;
CREATE TRIGGER task_action_item_batch_task_guard
    BEFORE INSERT OR UPDATE OF
        id,
        room_id,
        creator_id,
        action_item_batch_key,
        action_item_batch_index,
        created_at
    ON tasks
    FOR EACH ROW
    EXECUTE FUNCTION task_action_item_batch_task_guard();

CREATE OR REPLACE FUNCTION task_action_item_batch_complete_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    persisted_task_ids uuid[];
BEGIN
    SELECT COALESCE(
               array_agg(
                   task.id
                   ORDER BY task.action_item_batch_index
               ),
               ARRAY[]::uuid[]
           )
      INTO persisted_task_ids
      FROM tasks AS task
     WHERE task.creator_id = NEW.participant_id
       AND task.room_id = NEW.room_id
       AND task.action_item_batch_key = NEW.idempotency_key;

    IF persisted_task_ids IS DISTINCT FROM NEW.task_ids THEN
        RAISE EXCEPTION
            'action-item batch must commit its complete ordered task set'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'task_action_item_batch_complete_chk';
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS task_action_item_batch_complete_guard
    ON task_action_item_batches;
CREATE CONSTRAINT TRIGGER task_action_item_batch_complete_guard
    AFTER INSERT OR UPDATE
    ON task_action_item_batches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION task_action_item_batch_complete_guard();

COMMENT ON TABLE task_action_item_batches IS
    'Durable action-item idempotency receipts keyed by SHA-256 digest, including empty batches and original ordered task ids.';
COMMENT ON TRIGGER task_action_item_batch_task_guard ON tasks IS
    'Backstops current access, immutable batch identity, and exact receipt id/index containment while leaving ordinary tasks unchanged.';
