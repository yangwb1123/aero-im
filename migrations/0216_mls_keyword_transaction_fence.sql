-- Transaction and tenant-identity fences for the opaque MLS relay and keyword
-- alerts. MLS bytes remain opaque: this migration adds authorization metadata
-- and invariants only; it does not interpret or rewrite group state.

-- `updated_by` is nullable so retained pre-0216 rows and mixed-version writers
-- remain readable during a rolling upgrade. All 0216 production writes provide
-- it, and the trigger validates that actor against the current canonical room.
ALTER TABLE mls_groups
    ADD COLUMN IF NOT EXISTS updated_by uuid;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'mls_groups_updated_by_fkey'
           AND conrelid = 'mls_groups'::regclass
    ) THEN
        ALTER TABLE mls_groups
            ADD CONSTRAINT mls_groups_updated_by_fkey
            FOREIGN KEY (updated_by)
            REFERENCES participants (id)
            ON DELETE SET NULL
            NOT VALID;
    END IF;
END
$$;

ALTER TABLE mls_groups
    VALIDATE CONSTRAINT mls_groups_updated_by_fkey;

CREATE INDEX IF NOT EXISTS mls_groups_updated_by_idx
    ON mls_groups (updated_by)
    WHERE updated_by IS NOT NULL;

CREATE OR REPLACE FUNCTION mls_group_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_workspace uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.group_id IS DISTINCT FROM OLD.group_id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.ciphersuite IS DISTINCT FROM OLD.ciphersuite
       ) THEN
        RAISE EXCEPTION 'MLS group room and ciphersuite identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'mls_groups_identity_immutable_chk';
    END IF;

    IF NEW.epoch < 0
       OR (
           TG_OP = 'UPDATE'
           AND NEW.epoch < OLD.epoch
       ) THEN
        RAISE EXCEPTION 'MLS group epoch cannot move backwards'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'mls_groups_epoch_nondecreasing_chk';
    END IF;

    -- Nullable is the rolling-upgrade seam. New application writes always set
    -- updated_by; historical rows and old pods may retain NULL without changing
    -- or exposing the opaque state bytes.
    IF NEW.updated_by IS NOT NULL THEN
        SELECT workspace_id
          INTO canonical_workspace
          FROM rooms
         WHERE id = NEW.room_id;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               NEW.updated_by
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members
                WHERE room_id = NEW.room_id
                  AND participant_id = NEW.updated_by
           ) THEN
            RAISE EXCEPTION
                'MLS group writer lacks current canonical room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'mls_groups_updated_by_room_scope_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS mls_group_transaction_fence
    ON mls_groups;
CREATE TRIGGER mls_group_transaction_fence
    BEFORE INSERT OR UPDATE
    ON mls_groups
    FOR EACH ROW
    EXECUTE FUNCTION mls_group_transaction_fence();

COMMENT ON TRIGGER mls_group_transaction_fence
    ON mls_groups IS
    'Immutable room/ciphersuite, nondecreasing epoch, and lock-free current actor containment for opaque MLS state';

-- Migration 0037 intentionally shipped without FKs. Remove only orphan alert
-- metadata before adding cascading tenant/owner lifecycle edges.
DELETE FROM keyword_alerts AS alert
 WHERE NOT EXISTS (
           SELECT 1
             FROM participants AS participant
            WHERE participant.id = alert.participant_id
       )
    OR NOT EXISTS (
           SELECT 1
             FROM workspaces AS workspace
            WHERE workspace.id = alert.workspace_id
       );

-- Canonicalize retained keywords without allowing pre-normalization duplicates
-- to collide with the existing unique constraint.
DELETE FROM keyword_alerts
 WHERE btrim(keyword) = '';

WITH ranked AS (
    SELECT id,
           row_number() OVER (
               PARTITION BY
                   participant_id,
                   workspace_id,
                   left(lower(btrim(keyword)), 64)
               ORDER BY created_at, id
           ) AS duplicate_rank
      FROM keyword_alerts
)
DELETE FROM keyword_alerts AS alert
 USING ranked
 WHERE alert.id = ranked.id
   AND ranked.duplicate_rank > 1;

UPDATE keyword_alerts
   SET keyword = left(lower(btrim(keyword)), 64)
 WHERE keyword IS DISTINCT FROM left(lower(btrim(keyword)), 64);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'keyword_alerts_participant_fkey'
           AND conrelid = 'keyword_alerts'::regclass
    ) THEN
        ALTER TABLE keyword_alerts
            ADD CONSTRAINT keyword_alerts_participant_fkey
            FOREIGN KEY (participant_id)
            REFERENCES participants (id)
            ON DELETE CASCADE;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'keyword_alerts_workspace_fkey'
           AND conrelid = 'keyword_alerts'::regclass
    ) THEN
        ALTER TABLE keyword_alerts
            ADD CONSTRAINT keyword_alerts_workspace_fkey
            FOREIGN KEY (workspace_id)
            REFERENCES workspaces (id)
            ON DELETE CASCADE;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'keyword_alerts_keyword_canonical_chk'
           AND conrelid = 'keyword_alerts'::regclass
    ) THEN
        ALTER TABLE keyword_alerts
            ADD CONSTRAINT keyword_alerts_keyword_canonical_chk
            CHECK (
                keyword = lower(btrim(keyword))
                AND char_length(keyword) BETWEEN 1 AND 64
            );
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION keyword_alert_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.id IS DISTINCT FROM OLD.id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.keyword IS DISTINCT FROM OLD.keyword
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
       ) THEN
        RAISE EXCEPTION 'keyword alert identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'keyword_alerts_identity_immutable_chk';
    END IF;

    IF TG_OP = 'INSERT'
       AND NOT aero_participant_has_effective_workspace_access(
           NEW.workspace_id,
           NEW.participant_id
       ) THEN
        RAISE EXCEPTION
            'keyword alert owner lacks current effective workspace access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'keyword_alerts_owner_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS keyword_alert_transaction_fence
    ON keyword_alerts;
CREATE TRIGGER keyword_alert_transaction_fence
    BEFORE INSERT OR UPDATE
    ON keyword_alerts
    FOR EACH ROW
    EXECUTE FUNCTION keyword_alert_transaction_fence();

COMMENT ON TRIGGER keyword_alert_transaction_fence
    ON keyword_alerts IS
    'Canonical immutable subscription identity plus lock-free current owner containment; retained rows reactivate with restored access';
