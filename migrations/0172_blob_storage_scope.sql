-- 0172_blob_storage_scope.sql — Persist each blob's immutable storage location.
--
-- `workspaces.region_code` is mutable configuration: it decides where a future
-- upload is placed, but must never be used to locate bytes that already exist.
-- These two columns snapshot the authenticated workspace scope and the selected
-- storage backend at reservation time. Legacy rows remain NULL and are read from
-- the default backend for backward compatibility.

ALTER TABLE blobs
    ADD COLUMN IF NOT EXISTS workspace_id UUID,
    ADD COLUMN IF NOT EXISTS storage_region VARCHAR(16);

-- The old workspace-region migration documented blank as default. Normalize
-- such rows before tightening the write shape used by the management API.
UPDATE workspaces
   SET region_code = NULL
 WHERE region_code IS NOT NULL
   AND btrim(region_code) = '';

ALTER TABLE workspaces
    DROP CONSTRAINT IF EXISTS workspaces_region_code_shape,
    ADD CONSTRAINT workspaces_region_code_shape
        CHECK (region_code IS NULL OR (region_code = btrim(region_code) AND region_code <> ''));

ALTER TABLE blobs
    DROP CONSTRAINT IF EXISTS blobs_storage_region_shape,
    ADD CONSTRAINT blobs_storage_region_shape
        CHECK (
            storage_region IS NULL
            OR (storage_region = btrim(storage_region) AND storage_region <> '')
        );

-- Dedup is deliberately non-unique: concurrent reservations may temporarily
-- share a digest. The lookup still stays owner + workspace + region scoped, so
-- an upload can never reuse an object from another residency boundary.
CREATE INDEX IF NOT EXISTS blobs_owner_scope_sha256_idx
    ON blobs (
        owner_id,
        workspace_id,
        COALESCE(storage_region, 'default'),
        sha256
    )
    WHERE finalized_at IS NOT NULL AND sha256 IS NOT NULL;

COMMENT ON COLUMN blobs.workspace_id IS
    'Immutable authenticated workspace scope at reservation; NULL is a legacy/unscoped blob.';
COMMENT ON COLUMN blobs.storage_region IS
    'Immutable storage backend code at reservation; NULL means legacy default backend.';

CREATE OR REPLACE FUNCTION prevent_blob_storage_scope_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.workspace_id IS DISTINCT FROM NEW.workspace_id
       OR OLD.storage_region IS DISTINCT FROM NEW.storage_region THEN
        RAISE EXCEPTION 'blob storage scope is immutable'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS blobs_storage_scope_immutable ON blobs;
CREATE TRIGGER blobs_storage_scope_immutable
    BEFORE UPDATE OF workspace_id, storage_region ON blobs
    FOR EACH ROW
    EXECUTE FUNCTION prevent_blob_storage_scope_change();

-- Rolling-upgrade attachment boundary ---------------------------------------
--
-- Blob ids are UUID bits in Postgres and canonical Crockford-base32 ULIDs in
-- message JSON. The immutable conversion supports an expression index and lets
-- the database validate old application writes without trusting application
-- parsing.
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

-- Lock both the canonical table and the partition-cutover candidate before
-- auditing. Locks remain held through trigger installation, closing the race
-- where an old pod could insert a mismatched reference after the scan.
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
    -- Application validation owns the complete Block schema. This trigger is
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

        -- Opaque legacy ids with no blob row cannot disclose bytes. New service
        -- code rejects them; the database fence stops every existing blob from
        -- crossing its immutable tenant boundary.
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

-- A trigger attached only to `messages` follows it when the online cutover
-- renames it to `messages_old`. Guard the candidate parent now so promotion
-- cannot expose an unguarded live table.
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
