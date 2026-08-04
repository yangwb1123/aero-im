-- Database fences for migration-first rolling upgrades.
--
-- Migrations 0172, 0173, and 0175 may already be recorded in production.
-- Keep those immutable: this migration installs every compatibility fence
-- required while previous application binaries are still serving traffic.

-- -------------------------------------------------------------------------
-- Canvas: previous binaries omit client_op_id.
-- -------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fill_legacy_canvas_op_client_id()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.client_op_id IS NULL THEN
        NEW.client_op_id := NEW.id;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS canvas_ops_fill_legacy_client_id ON canvas_ops;
CREATE TRIGGER canvas_ops_fill_legacy_client_id
    BEFORE INSERT ON canvas_ops
    FOR EACH ROW
    EXECUTE FUNCTION fill_legacy_canvas_op_client_id();

-- -------------------------------------------------------------------------
-- Blob attachments: enforce the immutable room/workspace boundary even when a
-- previous server binary writes messages without the new repository guard.
-- -------------------------------------------------------------------------

-- Blob ids are UUID bits in Postgres and canonical Crockford-base32 ULIDs in a
-- message's JSON blocks.  This immutable conversion supports an expression
-- index and lets the trigger resolve a JSON reference without trusting the app.
CREATE OR REPLACE FUNCTION aero_uuid_to_ulid(input_uuid UUID)
RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
DECLARE
    raw_bits BIT(130);
    alphabet CONSTANT TEXT := '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
    result TEXT := '';
    position INTEGER;
    digit INTEGER;
BEGIN
    raw_bits := B'00'
        || (('x' || replace(input_uuid::text, '-', ''))::BIT(128));
    FOR position IN 0..25 LOOP
        digit := substring(raw_bits FROM position * 5 + 1 FOR 5)::INTEGER;
        result := result || substr(alphabet, digit + 1, 1);
    END LOOP;
    RETURN result;
END;
$$;

CREATE UNIQUE INDEX IF NOT EXISTS blobs_public_ulid_idx
    ON blobs (aero_uuid_to_ulid(id));

-- Lock each possible live message relation before auditing it.  The locks are
-- retained until this migration commits, closing the scan/install race with an
-- old application writer.  During the partition cutover both relations can
-- contain readable rows, so both must be clean before either trigger is added.
DO $$
DECLARE
    message_relation TEXT;
    violation_count BIGINT;
BEGIN
    FOREACH message_relation IN ARRAY ARRAY['messages', 'messages_partitioned']
    LOOP
        IF to_regclass(message_relation) IS NULL THEN
            CONTINUE;
        END IF;

        EXECUTE format(
            'LOCK TABLE %I IN SHARE ROW EXCLUSIVE MODE',
            message_relation
        );

        EXECUTE format(
            $audit$
            SELECT count(*)
              FROM %I AS message
              JOIN rooms AS room
                ON room.id = message.room_id
              CROSS JOIN LATERAL jsonb_array_elements(
                  CASE
                      WHEN jsonb_typeof(message.blocks) = 'array'
                          THEN message.blocks
                      ELSE '[]'::jsonb
                  END
              ) AS element
              JOIN blobs AS blob
                ON aero_uuid_to_ulid(blob.id) = upper(element ->> 'blob_id')
             WHERE element ->> 'type' IN ('file', 'voice')
               AND blob.workspace_id IS DISTINCT FROM room.workspace_id
            $audit$,
            message_relation
        )
        INTO violation_count;

        IF violation_count > 0 THEN
            RAISE EXCEPTION
                'blob workspace audit found % mismatched attachment reference(s) in %',
                violation_count,
                message_relation
                USING
                    ERRCODE = 'check_violation',
                    HINT = 'Quarantine or replace the mismatched blocks, then rerun the migration chain; see docs/runbooks/rolling-upgrade-0172-0176.md';
        END IF;
    END LOOP;
END
$$;

CREATE OR REPLACE FUNCTION enforce_message_blob_workspace_scope()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    room_workspace UUID;
    attachment_ref TEXT;
    matched_blob UUID;
    blob_workspace UUID;
BEGIN
    -- Application validation owns the complete Block schema.  This trigger is
    -- deliberately limited to the two attachment block variants.
    IF jsonb_typeof(NEW.blocks) IS DISTINCT FROM 'array' THEN
        RETURN NEW;
    END IF;

    SELECT workspace_id
      INTO room_workspace
      FROM rooms
     WHERE id = NEW.room_id;

    FOR attachment_ref IN
        SELECT upper(element ->> 'blob_id')
          FROM jsonb_array_elements(NEW.blocks) AS element
         WHERE element ->> 'type' IN ('file', 'voice')
    LOOP
        matched_blob := NULL;
        blob_workspace := NULL;
        SELECT blob.id, blob.workspace_id
          INTO matched_blob, blob_workspace
          FROM blobs AS blob
         WHERE aero_uuid_to_ulid(blob.id) = attachment_ref
         FOR SHARE;

        -- A non-resolving legacy reference cannot disclose bytes. New service
        -- code rejects it before INSERT; the database fence only needs to stop
        -- an existing blob from crossing its immutable tenant boundary.
        IF matched_blob IS NOT NULL
           AND blob_workspace IS DISTINCT FROM room_workspace THEN
            RAISE EXCEPTION
                'message attachment is unavailable in the target workspace'
                USING ERRCODE = 'check_violation';
        END IF;
    END LOOP;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS messages_enforce_blob_workspace_scope ON messages;
CREATE TRIGGER messages_enforce_blob_workspace_scope
    BEFORE INSERT OR UPDATE OF room_id, blocks ON messages
    FOR EACH ROW
    EXECUTE FUNCTION enforce_message_blob_workspace_scope();

-- `messages_partitioned` is the shadow/live candidate used by the online
-- cutover.  A trigger attached only to `messages` would follow that relation
-- when it is renamed to `messages_old`, leaving the promoted table unguarded.
DO $$
BEGIN
    IF to_regclass('messages_partitioned') IS NOT NULL THEN
        DROP TRIGGER IF EXISTS messages_enforce_blob_workspace_scope
            ON messages_partitioned;
        CREATE TRIGGER messages_enforce_blob_workspace_scope
            BEFORE INSERT OR UPDATE OF room_id, blocks ON messages_partitioned
            FOR EACH ROW
            EXECUTE FUNCTION enforce_message_blob_workspace_scope();
    END IF;
END
$$;

-- -------------------------------------------------------------------------
-- AI profiles: old binaries query only by participant_id and cannot safely
-- select among tenant rows.  Move the real data behind a new relation name and
-- leave the legacy name as an empty compatibility view.
-- -------------------------------------------------------------------------

DO $$
BEGIN
    IF to_regclass('participant_ai_profiles_scoped') IS NULL
       AND EXISTS (
           SELECT 1
             FROM pg_class
            WHERE oid = to_regclass('participant_ai_profiles')
              AND relkind IN ('r', 'p')
       ) THEN
        ALTER TABLE participant_ai_profiles
            RENAME TO participant_ai_profiles_scoped;
    END IF;
END
$$;

DROP VIEW IF EXISTS participant_ai_profiles;
CREATE VIEW participant_ai_profiles AS
SELECT participant_id, workspace_id, topics, preferences, summary, updated_at
  FROM participant_ai_profiles_scoped
 WHERE FALSE
WITH LOCAL CHECK OPTION;

COMMENT ON VIEW participant_ai_profiles IS
    'Fail-closed rolling-upgrade fence for pre-tenant AI-profile binaries.';

-- Reassert the historical-erasure cleanup for databases that recorded an
-- earlier 0173 revision before this defence-in-depth migration shipped.
DELETE FROM participant_ai_profiles_scoped AS profile
USING participants AS subject
 WHERE profile.participant_id = subject.id
   AND subject.deleted_at IS NOT NULL;

DO $$
DECLARE
    table_grant RECORD;
    grantee_sql TEXT;
BEGIN
    FOR table_grant IN
        SELECT grantee, privilege_type
          FROM information_schema.role_table_grants
         WHERE table_schema = current_schema()
           AND table_name = 'participant_ai_profiles_scoped'
           AND privilege_type IN ('SELECT', 'INSERT', 'UPDATE', 'DELETE')
    LOOP
        grantee_sql := CASE
            WHEN table_grant.grantee = 'PUBLIC' THEN 'PUBLIC'
            ELSE format('%I', table_grant.grantee)
        END;
        EXECUTE format(
            'GRANT %s ON participant_ai_profiles TO %s',
            table_grant.privilege_type,
            grantee_sql
        );
    END LOOP;
END
$$;

CREATE OR REPLACE FUNCTION erase_scoped_ai_profiles_on_participant_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM participant_ai_profiles_scoped
         WHERE participant_id = NEW.id;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS participants_erase_scoped_ai_profiles ON participants;
CREATE TRIGGER participants_erase_scoped_ai_profiles
    AFTER UPDATE OF deleted_at ON participants
    FOR EACH ROW
    EXECUTE FUNCTION erase_scoped_ai_profiles_on_participant_tombstone();
