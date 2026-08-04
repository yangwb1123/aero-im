-- Bind both historical custom-emoji stores to usable image blobs in the same
-- immutable workspace scope.
--
-- Historical metadata is repaired before the trigger is installed: a valid
-- ordinary cleanup reservation is cancelled because emoji are live references;
-- broken, unscoped, cross-tenant, unfinished, non-image, and force-delete
-- references are removed rather than exposing an unusable tenant asset. DELETE
-- never invokes the guard.

-- Keep old application writers out between cleanup and trigger installation.
LOCK TABLE custom_emoji, workspace_emoji, blobs, blob_gc_queue
    IN SHARE ROW EXCLUSIVE MODE;

DELETE FROM blob_gc_queue queued
 WHERE NOT queued.force_delete
   AND (
       EXISTS (
           SELECT 1
             FROM custom_emoji emoji
             JOIN blobs blob ON blob.id = emoji.blob_id
            WHERE emoji.blob_id = queued.blob_id
              AND blob.workspace_id = emoji.workspace_id
              AND blob.kind = 'image'
              AND blob.finalized_at IS NOT NULL
       )
       OR EXISTS (
           SELECT 1
             FROM workspace_emoji emoji
             JOIN blobs blob ON blob.id = emoji.blob_id
            WHERE emoji.blob_id = queued.blob_id
              AND blob.workspace_id = emoji.workspace_id
              AND blob.kind = 'image'
              AND blob.finalized_at IS NOT NULL
       )
   );

DELETE FROM custom_emoji emoji
 WHERE NOT EXISTS (
       SELECT 1
         FROM blobs blob
        WHERE blob.id = emoji.blob_id
          AND blob.workspace_id = emoji.workspace_id
          AND blob.kind = 'image'
          AND blob.finalized_at IS NOT NULL
          AND NOT EXISTS (
              SELECT 1
                FROM blob_gc_queue queued
               WHERE queued.blob_id = blob.id
          )
   );

DELETE FROM workspace_emoji emoji
 WHERE NOT EXISTS (
       SELECT 1
         FROM blobs blob
        WHERE blob.id = emoji.blob_id
          AND blob.workspace_id = emoji.workspace_id
          AND blob.kind = 'image'
          AND blob.finalized_at IS NOT NULL
          AND NOT EXISTS (
              SELECT 1
                FROM blob_gc_queue queued
               WHERE queued.blob_id = blob.id
          )
   );

CREATE OR REPLACE FUNCTION aero_validate_emoji_blob_containment()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    blob_workspace uuid;
    blob_kind text;
    blob_finalized_at timestamptz;
    constraint_name text;
BEGIN
    constraint_name := TG_TABLE_NAME || '_blob_workspace_containment';

    IF TG_OP = 'UPDATE'
       AND OLD.workspace_id IS DISTINCT FROM NEW.workspace_id THEN
        RAISE EXCEPTION 'custom emoji workspace scope is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = TG_TABLE_NAME || '_workspace_immutable';
    END IF;

    -- Match the same availability boundary used by BlobRepo. FOR UPDATE shares
    -- the attachment/GC reservation fence: a normal cleanup that won first
    -- leaves a queue row and this write fails; a successful emoji registration
    -- wins before a later reference-aware cleanup decision.
    SELECT blob.workspace_id, blob.kind, blob.finalized_at
      INTO blob_workspace, blob_kind, blob_finalized_at
      FROM blobs blob
     WHERE blob.id = NEW.blob_id
       FOR UPDATE;

    IF NOT FOUND
       OR blob_workspace IS DISTINCT FROM NEW.workspace_id
       OR blob_kind <> 'image'
       OR blob_finalized_at IS NULL
       OR EXISTS (
           SELECT 1
             FROM blob_gc_queue queued
            WHERE queued.blob_id = NEW.blob_id
       ) THEN
        RAISE EXCEPTION
            'custom emoji blob is unavailable in workspace %',
            NEW.workspace_id
            USING ERRCODE = '23514',
                  CONSTRAINT = constraint_name;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS custom_emoji_blob_containment ON custom_emoji;
CREATE TRIGGER custom_emoji_blob_containment
    BEFORE INSERT OR UPDATE OF workspace_id, blob_id
    ON custom_emoji
    FOR EACH ROW
    EXECUTE FUNCTION aero_validate_emoji_blob_containment();

DROP TRIGGER IF EXISTS workspace_emoji_blob_containment ON workspace_emoji;
CREATE TRIGGER workspace_emoji_blob_containment
    BEFORE INSERT OR UPDATE OF workspace_id, blob_id
    ON workspace_emoji
    FOR EACH ROW
    EXECUTE FUNCTION aero_validate_emoji_blob_containment();

-- `workspace_emoji` already cascades when GDPR/expiry force-deletes a blob.
-- Bring the older table to the same lifecycle contract. Ordinary cleanup sees
-- emoji as live references in BlobRepo and is cancelled; forced deletion wins
-- and removes the now-broken emoji metadata instead of leaving GC stuck behind
-- a NO ACTION foreign key.
ALTER TABLE custom_emoji
    DROP CONSTRAINT IF EXISTS custom_emoji_blob_id_fkey;
ALTER TABLE custom_emoji
    ADD CONSTRAINT custom_emoji_blob_id_fkey
    FOREIGN KEY (blob_id) REFERENCES blobs(id)
    ON DELETE CASCADE
    NOT VALID;
ALTER TABLE custom_emoji
    VALIDATE CONSTRAINT custom_emoji_blob_id_fkey;

COMMENT ON FUNCTION aero_validate_emoji_blob_containment() IS
    'Requires custom emoji references to target a finalized, non-GC-queued image blob in the same immutable workspace; migration 0204 removes unsafe historical metadata first.';
