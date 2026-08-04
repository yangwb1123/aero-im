-- Preserve collaborative tasks while erasing their creator's private
-- action-item idempotency receipt.
--
-- Migration 0224 intentionally linked batch tasks to their durable receipt with
-- ON DELETE CASCADE. Participant erasure tombstones rather than hard-deletes the
-- creator, so it must first detach only the batch key/index metadata and then
-- delete the receipt/digest. Mixed-version safety is fail-closed:
--   * a new writer against pre-0226 triggers cannot detach batch identity;
--   * an old writer against this migration cannot delete a receipt without the
--     explicit transaction-local erasure actor.

CREATE OR REPLACE FUNCTION aero_participant_erasure_actor()
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    SELECT NULLIF(
        current_setting('aero.participant_erasure_actor', true),
        ''
    )::uuid
$$;

CREATE OR REPLACE FUNCTION task_action_item_batch_task_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    expected_task_ids uuid[];
    request_actor uuid;
    erasure_detach boolean;
BEGIN
    erasure_detach :=
        TG_OP = 'UPDATE'
        AND OLD.action_item_batch_key IS NOT NULL
        AND OLD.action_item_batch_index IS NOT NULL
        AND NEW.action_item_batch_key IS NULL
        AND NEW.action_item_batch_index IS NULL;

    IF erasure_detach THEN
        -- The erasure exception is deliberately exact: no task projection or
        -- lifecycle field may hitchhike on the metadata detach.
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.creator_id IS DISTINCT FROM OLD.creator_id
           OR NEW.assignee_id IS DISTINCT FROM OLD.assignee_id
           OR NEW.title IS DISTINCT FROM OLD.title
           OR NEW.source_message_id IS DISTINCT FROM OLD.source_message_id
           OR NEW.status IS DISTINCT FROM OLD.status
           OR NEW.due_at IS DISTINCT FROM OLD.due_at
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
           OR NEW.updated_at IS DISTINCT FROM OLD.updated_at THEN
            RAISE EXCEPTION
                'action-item erasure may only detach batch metadata'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tasks_action_item_batch_erasure_exact_chk';
        END IF;

        request_actor := aero_participant_erasure_actor();
        IF request_actor IS NULL OR request_actor <> OLD.creator_id THEN
            RAISE EXCEPTION
                'action-item batch detach requires erasure actor context'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tasks_action_item_batch_erasure_actor_chk';
        END IF;
        IF NOT EXISTS (
            SELECT 1
              FROM participants AS participant
             WHERE participant.id = OLD.creator_id
               AND participant.deleted_at IS NOT NULL
        ) THEN
            RAISE EXCEPTION
                'action-item batch detach requires a tombstoned creator'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tasks_action_item_batch_erasure_tombstone_chk';
        END IF;
        RETURN NEW;
    END IF;

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

CREATE OR REPLACE FUNCTION task_action_item_batch_receipt_delete_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request_actor uuid;
BEGIN
    -- Preserve genuine parent-key cascades. During a participant or room
    -- cascade the parent row is no longer visible to this child BEFORE trigger.
    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = OLD.participant_id
    )
       OR NOT EXISTS (
           SELECT 1
             FROM rooms AS room
            WHERE room.id = OLD.room_id
       ) THEN
        RETURN OLD;
    END IF;

    request_actor := aero_participant_erasure_actor();
    IF request_actor IS NULL OR request_actor <> OLD.participant_id THEN
        RAISE EXCEPTION
            'action-item receipt deletion requires erasure actor context'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'task_action_item_batch_delete_actor_chk';
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = OLD.participant_id
           AND participant.deleted_at IS NOT NULL
    ) THEN
        RAISE EXCEPTION
            'action-item receipt deletion requires a tombstoned creator'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'task_action_item_batch_delete_tombstone_chk';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM tasks AS task
         WHERE task.creator_id = OLD.participant_id
           AND task.room_id = OLD.room_id
           AND task.action_item_batch_key = OLD.idempotency_key
    ) THEN
        RAISE EXCEPTION
            'action-item receipt deletion requires detached tasks'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'task_action_item_batch_delete_detached_chk';
    END IF;

    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS task_action_item_batch_receipt_delete_guard
    ON task_action_item_batches;
CREATE TRIGGER task_action_item_batch_receipt_delete_guard
    BEFORE DELETE
    ON task_action_item_batches
    FOR EACH ROW
    EXECUTE FUNCTION task_action_item_batch_receipt_delete_guard();

COMMENT ON FUNCTION aero_participant_erasure_actor() IS
    'Transaction-local participant id authorized to detach and erase its own action-item batch receipts.';
COMMENT ON TRIGGER task_action_item_batch_task_guard ON tasks IS
    'Preserves immutable batch identity except an exact key/index detach for the matching tombstoned erasure actor.';
COMMENT ON TRIGGER task_action_item_batch_receipt_delete_guard
    ON task_action_item_batches IS
    'Prevents receipt cascade loss: erasure must tombstone its creator and detach every linked task before deleting the digest.';
