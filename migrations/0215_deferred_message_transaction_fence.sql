-- Commit-time authorization and raw-SQL fences for one-shot and recurring
-- deferred messages. Historical rows remain settleable after membership
-- revocation: effective access is required only for INSERT, while immutable
-- identity and delivery-state transitions are enforced on every UPDATE.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'scheduled_messages'::regclass
           AND conname = 'scheduled_messages_blocks_shape_chk'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_blocks_shape_chk
            CHECK (
                jsonb_typeof(blocks) = 'array'
                AND jsonb_array_length(blocks) > 0
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'recurring_messages'::regclass
           AND conname = 'recurring_messages_blocks_shape_chk'
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_blocks_shape_chk
            CHECK (
                jsonb_typeof(blocks) = 'array'
                AND jsonb_array_length(blocks) > 0
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'recurring_messages'::regclass
           AND conname = 'recurring_messages_cadence_chk'
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_cadence_chk
            CHECK (cadence IN ('hourly', 'daily', 'weekly'))
            NOT VALID;
    END IF;

    -- These are NOT VALID so a rolling upgrade does not rewrite or reject
    -- historical orphans. PostgreSQL still enforces them for every new row and
    -- installs the desired cascade behavior.
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'recurring_messages'::regclass
           AND conname = 'recurring_messages_room_id_fkey'
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_room_id_fkey
            FOREIGN KEY (room_id)
            REFERENCES rooms (id)
            ON DELETE CASCADE
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'recurring_messages'::regclass
           AND conname = 'recurring_messages_sender_id_fkey'
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_sender_id_fkey
            FOREIGN KEY (sender_id)
            REFERENCES participants (id)
            ON DELETE CASCADE
            NOT VALID;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION scheduled_messages_scope_state_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_room uuid;
    parent_deleted timestamptz;
    reminder_anchor text;
    reminder_actor text;
    reminder_room uuid;
    reminder_deleted timestamptz;
    validate_reply boolean := false;
    validate_blocks boolean := false;
BEGIN
    IF jsonb_typeof(NEW.blocks) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'scheduled message blocks must be a non-empty array'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'scheduled_messages_blocks_shape_chk';
    END IF;
    IF jsonb_array_length(NEW.blocks) = 0 THEN
        RAISE EXCEPTION 'scheduled message blocks must be a non-empty array'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'scheduled_messages_blocks_shape_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        validate_reply := true;
        validate_blocks := true;
        IF NEW.delivery_status <> 'pending'
           OR NEW.attempts <> 0
           OR NEW.delivery_generation <> 0
           OR NEW.claim_token IS NOT NULL
           OR NEW.claimed_at IS NOT NULL
           OR NEW.lease_expires_at IS NOT NULL
           OR NEW.delivered_at IS NOT NULL
           OR NEW.canceled_at IS NOT NULL
           OR NEW.dead_at IS NOT NULL THEN
            RAISE EXCEPTION 'scheduled message must start pending and unclaimed'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_initial_state_chk';
        END IF;

        IF NOT aero_effective_room_access(
            NEW.room_id,
            NEW.sender_id,
            NULL
        ) THEN
            RAISE EXCEPTION 'scheduled message sender lacks effective room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_sender_scope_chk';
        END IF;
    ELSE
        validate_reply := NEW.reply_to IS DISTINCT FROM OLD.reply_to;
        validate_blocks := NEW.blocks IS DISTINCT FROM OLD.blocks;
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.sender_id IS DISTINCT FROM OLD.sender_id
           OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
            RAISE EXCEPTION 'scheduled message room/sender identity is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_identity_immutable_chk';
        END IF;

        -- Payload edits are permitted only before any worker has observed the
        -- row. The older delivery fence also guards this after attempts > 0;
        -- this condition closes the raw-SQL gap around claimed/terminal rows.
        IF (
            NEW.blocks IS DISTINCT FROM OLD.blocks
            OR NEW.reply_to IS DISTINCT FROM OLD.reply_to
            OR NEW.scheduled_at IS DISTINCT FROM OLD.scheduled_at
        ) AND NOT (
            OLD.delivery_status = 'pending'
            AND NEW.delivery_status = 'pending'
            AND OLD.attempts = 0
            AND NEW.attempts = 0
            AND OLD.delivery_generation = 0
            AND NEW.delivery_generation = 0
            AND OLD.claim_token IS NULL
            AND NEW.claim_token IS NULL
            AND OLD.canceled_at IS NULL
            AND NEW.canceled_at IS NULL
        ) AND NOT (
            NEW.blocks IS NOT DISTINCT FROM OLD.blocks
            AND NEW.scheduled_at IS NOT DISTINCT FROM OLD.scheduled_at
            AND OLD.reply_to IS NOT NULL
            AND NEW.reply_to IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM messages AS parent
                 WHERE parent.id = OLD.reply_to
                   AND parent.room_id = OLD.room_id
            )
        ) THEN
            RAISE EXCEPTION 'scheduled message payload is not editable in this state'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_payload_state_chk';
        END IF;

        IF NEW.delivery_status IS DISTINCT FROM OLD.delivery_status
           AND NOT (
               (OLD.delivery_status = 'pending'
                   AND NEW.delivery_status IN ('claimed', 'canceled'))
               OR (OLD.delivery_status = 'claimed'
                   AND NEW.delivery_status IN ('pending', 'delivered', 'dead'))
               OR (OLD.delivery_status = 'dead'
                   AND NEW.delivery_status IN ('pending', 'canceled'))
           ) THEN
            RAISE EXCEPTION 'illegal scheduled message delivery transition'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_state_transition_chk';
        END IF;

        IF NEW.attempts IS DISTINCT FROM OLD.attempts
           AND NOT (
               NEW.delivery_status = 'claimed'
               AND NEW.attempts = OLD.attempts + 1
               AND NEW.claim_token IS DISTINCT FROM OLD.claim_token
           )
           AND NOT (
               OLD.delivery_status = 'dead'
               AND NEW.delivery_status = 'pending'
               AND NEW.delivery_generation = OLD.delivery_generation + 1
               AND NEW.attempts = 0
           ) THEN
            RAISE EXCEPTION 'scheduled message attempts may only advance on claim'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_attempt_transition_chk';
        END IF;

        IF NEW.claim_token IS DISTINCT FROM OLD.claim_token
           AND NEW.delivery_status = 'claimed'
           AND NOT (
               NEW.claim_token IS NOT NULL
               AND NEW.attempts = OLD.attempts + 1
               AND (
                   OLD.delivery_status = 'pending'
                   OR (
                       OLD.delivery_status = 'claimed'
                       AND OLD.lease_expires_at <= NEW.claimed_at
                   )
               )
           ) THEN
            RAISE EXCEPTION 'scheduled message claim replacement is not fenced'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_claim_transition_chk';
        END IF;

        IF OLD.delivery_status = 'claimed'
           AND NEW.delivery_status = 'claimed'
           AND NEW.claim_token IS NOT DISTINCT FROM OLD.claim_token
           AND (
               NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
               OR NEW.lease_expires_at IS DISTINCT FROM OLD.lease_expires_at
           ) THEN
            RAISE EXCEPTION 'scheduled message lease cannot be renewed in place'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_claim_transition_chk';
        END IF;
    END IF;

    IF validate_reply AND NEW.reply_to IS NOT NULL THEN
        SELECT room_id, deleted_at
          INTO parent_room, parent_deleted
          FROM messages
         WHERE id = NEW.reply_to
         FOR SHARE;
        IF NOT FOUND
           OR parent_room <> NEW.room_id
           OR parent_deleted IS NOT NULL THEN
            RAISE EXCEPTION 'scheduled reply must reference a live same-room message'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_messages_live_reply_chk';
        END IF;
    END IF;

    IF validate_blocks THEN
        FOR reminder_anchor, reminder_actor IN
            SELECT element -> 'payload' ->> 'message_id',
                   element -> 'payload' ->> 'by'
              FROM jsonb_array_elements(NEW.blocks) AS element
             WHERE element ->> 'type' = 'card'
               AND element ->> 'schema' = 'message_reminder'
        LOOP
            IF reminder_anchor IS NULL
               OR upper(reminder_actor) IS DISTINCT FROM
                  aero_uuid_to_ulid(NEW.sender_id) THEN
                RAISE EXCEPTION 'message reminder actor/anchor is invalid'
                    USING ERRCODE = '23514',
                          CONSTRAINT = 'scheduled_messages_reminder_identity_chk';
            END IF;
            SELECT room_id, deleted_at
              INTO reminder_room, reminder_deleted
              FROM messages
             WHERE aero_uuid_to_ulid(id) = upper(reminder_anchor)
             FOR SHARE;
            IF NOT FOUND
               OR reminder_room <> NEW.room_id
               OR reminder_deleted IS NOT NULL THEN
                RAISE EXCEPTION 'message reminder must reference a live same-room message'
                    USING ERRCODE = '23514',
                          CONSTRAINT = 'scheduled_messages_live_reminder_chk';
            END IF;
        END LOOP;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS scheduled_messages_scope_state_guard
    ON scheduled_messages;
CREATE TRIGGER scheduled_messages_scope_state_guard
    BEFORE INSERT OR UPDATE
    ON scheduled_messages
    FOR EACH ROW
    EXECUTE FUNCTION scheduled_messages_scope_state_guard();

COMMENT ON TRIGGER scheduled_messages_scope_state_guard ON scheduled_messages IS
    'Requires effective sender access at creation; freezes identity, live reply/reminder containment, and legal fenced delivery transitions while allowing historical settlement after revocation.';

CREATE OR REPLACE FUNCTION recurring_messages_scope_state_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF jsonb_typeof(NEW.blocks) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'recurring message blocks must be a non-empty array'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_blocks_shape_chk';
    END IF;
    IF jsonb_array_length(NEW.blocks) = 0 THEN
        RAISE EXCEPTION 'recurring message blocks must be a non-empty array'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_blocks_shape_chk';
    END IF;

    IF NEW.cadence NOT IN ('hourly', 'daily', 'weekly') THEN
        RAISE EXCEPTION 'recurring message cadence is invalid'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_cadence_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF NOT NEW.active
           OR NEW.claim_token IS NOT NULL
           OR NEW.claimed_at IS NOT NULL
           OR NEW.lease_expires_at IS NOT NULL
           OR NEW.delivery_key IS NOT NULL
           OR NEW.delivery_payload IS NOT NULL
           OR NEW.delivery_attempts <> 0
           OR NEW.retry_at IS NOT NULL
           OR NEW.last_error IS NOT NULL
           OR NEW.last_sent_at IS NOT NULL
           OR NEW.dead_at IS NOT NULL THEN
            RAISE EXCEPTION 'recurring message must start active and unclaimed'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'recurring_messages_initial_state_chk';
        END IF;
        IF NOT aero_effective_room_access(
            NEW.room_id,
            NEW.sender_id,
            NULL
        ) THEN
            RAISE EXCEPTION 'recurring message sender lacks effective room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'recurring_messages_sender_scope_chk';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.room_id IS DISTINCT FROM OLD.room_id
       OR NEW.sender_id IS DISTINCT FROM OLD.sender_id
       OR NEW.blocks IS DISTINCT FROM OLD.blocks
       OR NEW.cadence IS DISTINCT FROM OLD.cadence
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'recurring message room/sender/payload identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_identity_immutable_chk';
    END IF;

    IF NOT OLD.active AND NEW.active THEN
        RAISE EXCEPTION 'inactive recurring message cannot be reactivated'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_active_transition_chk';
    END IF;

    IF NEW.claim_token IS DISTINCT FROM OLD.claim_token
       AND NEW.claim_token IS NOT NULL
       AND NOT (
           NEW.active
           AND NEW.delivery_attempts = OLD.delivery_attempts + 1
           AND (
               OLD.claim_token IS NULL
               OR OLD.lease_expires_at <= NEW.claimed_at
           )
       ) THEN
        RAISE EXCEPTION 'recurring message claim replacement is not fenced'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_claim_transition_chk';
    END IF;

    IF NEW.delivery_attempts IS DISTINCT FROM OLD.delivery_attempts
       AND NOT (
           NEW.claim_token IS NOT NULL
           AND NEW.delivery_attempts = OLD.delivery_attempts + 1
           AND NEW.claim_token IS DISTINCT FROM OLD.claim_token
       )
       AND NOT (
           NEW.delivery_attempts = 0
           AND NEW.claim_token IS NULL
           AND (
               NOT NEW.active
               OR (
                   OLD.claim_token IS NOT NULL
                   AND NEW.next_run > OLD.next_run
               )
           )
       ) THEN
        RAISE EXCEPTION 'recurring message attempts changed outside claim/settlement'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_attempt_transition_chk';
    END IF;

    IF NEW.next_run IS DISTINCT FROM OLD.next_run
       AND NOT (
           OLD.active
           AND NEW.active
           AND OLD.claim_token IS NOT NULL
           AND NEW.claim_token IS NULL
           AND NEW.delivery_attempts = 0
           AND NEW.next_run > OLD.next_run
           AND NEW.last_sent_at IS NOT NULL
       ) THEN
        RAISE EXCEPTION 'recurring next_run may advance only after fenced success'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_next_run_transition_chk';
    END IF;

    IF OLD.delivery_key IS NOT NULL
       AND NEW.delivery_key IS NOT NULL
       AND NEW.delivery_key IS DISTINCT FROM OLD.delivery_key THEN
        RAISE EXCEPTION 'recurring occurrence delivery key is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;
    IF OLD.delivery_key IS NULL
       AND NEW.delivery_key IS NOT NULL
       AND NEW.claim_token IS NULL THEN
        RAISE EXCEPTION 'recurring occurrence key requires an owned claim'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;
    IF OLD.delivery_key IS NOT NULL
       AND NEW.delivery_key IS NULL
       AND NOT (
           NEW.claim_token IS NULL
           AND (OLD.claim_token IS NOT NULL OR NOT NEW.active)
       ) THEN
        RAISE EXCEPTION 'recurring occurrence key cleared outside settlement'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;
    IF OLD.delivery_payload IS NOT NULL
       AND NEW.delivery_payload IS NOT NULL
       AND NEW.delivery_payload IS DISTINCT FROM OLD.delivery_payload THEN
        RAISE EXCEPTION 'recurring occurrence payload is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;
    IF OLD.delivery_payload IS NULL
       AND NEW.delivery_payload IS NOT NULL
       AND NEW.claim_token IS NULL THEN
        RAISE EXCEPTION 'recurring occurrence payload requires an owned claim'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;
    IF OLD.delivery_payload IS NOT NULL
       AND NEW.delivery_payload IS NULL
       AND NOT (
           NEW.claim_token IS NULL
           AND (OLD.claim_token IS NOT NULL OR NOT NEW.active)
       ) THEN
        RAISE EXCEPTION 'recurring occurrence payload cleared outside settlement'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_occurrence_identity_chk';
    END IF;

    IF OLD.claim_token IS NOT NULL
       AND NEW.claim_token IS NOT DISTINCT FROM OLD.claim_token
       AND (
           NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
           OR NEW.lease_expires_at IS DISTINCT FROM OLD.lease_expires_at
       ) THEN
        RAISE EXCEPTION 'recurring message lease cannot be renewed in place'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_claim_transition_chk';
    END IF;

    IF NOT NEW.active AND NEW.claim_token IS NOT NULL THEN
        RAISE EXCEPTION 'inactive recurring message cannot retain a delivery lease'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'recurring_messages_active_transition_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS recurring_messages_scope_state_guard
    ON recurring_messages;
CREATE TRIGGER recurring_messages_scope_state_guard
    BEFORE INSERT OR UPDATE
    ON recurring_messages
    FOR EACH ROW
    EXECUTE FUNCTION recurring_messages_scope_state_guard();

COMMENT ON TRIGGER recurring_messages_scope_state_guard ON recurring_messages IS
    'Requires effective sender access at creation and freezes owner/room/occurrence identity plus ABA-safe claim and cadence transitions without blocking historical settlement.';
