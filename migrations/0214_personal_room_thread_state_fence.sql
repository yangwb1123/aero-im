-- Transaction and tenant fences for private per-room/per-thread user state.
--
-- These tables were introduced as owner-scoped UI projections, before current
-- effective-access fencing became a storage invariant. Canonical repository
-- writes now hold the workspace -> room -> membership lock chain through the
-- mutation. The triggers below protect direct SQL and older writers, while
-- cleanup hooks prevent stale notification routing after room departure or a
-- thread-root tombstone.

LOCK TABLE message_drafts,
           channel_mutes,
           channel_notification_prefs,
           channel_favorites,
           thread_subscriptions,
           thread_mutes,
           thread_notification_prefs,
           thread_read_state
    IN SHARE ROW EXCLUSIVE MODE;

-- A soft-deleted reply target must not keep a composer draft attached to an
-- invisible thread. Preserve the draft itself and only clear its reply edge.
UPDATE message_drafts AS draft
   SET reply_to = NULL
 WHERE draft.reply_to IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM messages AS parent
        WHERE parent.id = draft.reply_to
          AND parent.room_id = draft.room_id
          AND parent.deleted_at IS NULL
   );

-- Remove legacy room state that cannot currently be reached by its owner.
DELETE FROM message_drafts AS state
 WHERE NOT aero_effective_room_access(
               state.room_id,
               state.participant_id,
               NULL
           );
DELETE FROM channel_mutes AS state
 WHERE NOT aero_effective_room_access(
               state.room_id,
               state.participant_id,
               NULL
           );
DELETE FROM channel_notification_prefs AS state
 WHERE NOT aero_effective_room_access(
               state.room_id,
               state.participant_id,
               NULL
           );
DELETE FROM channel_favorites AS state
 WHERE NOT aero_effective_room_access(
               state.room_id,
               state.participant_id,
               NULL
           );

-- Notification-routing projections only apply to live canonical roots.
DELETE FROM thread_subscriptions AS state
 WHERE NOT EXISTS (
           SELECT 1
             FROM messages AS root
            WHERE root.id = state.root_message_id
              AND root.reply_to IS NULL
              AND root.deleted_at IS NULL
              AND aero_effective_room_access(
                      root.room_id,
                      state.participant_id,
                      NULL
                  )
       );
DELETE FROM thread_mutes AS state
 WHERE NOT EXISTS (
           SELECT 1
             FROM messages AS root
            WHERE root.id = state.root_message_id
              AND root.reply_to IS NULL
              AND root.deleted_at IS NULL
              AND aero_effective_room_access(
                      root.room_id,
                      state.participant_id,
                      NULL
                  )
       );
DELETE FROM thread_notification_prefs AS state
 WHERE NOT EXISTS (
           SELECT 1
             FROM messages AS root
            WHERE root.id = state.root_message_id
              AND root.reply_to IS NULL
              AND root.deleted_at IS NULL
              AND aero_effective_room_access(
                      root.room_id,
                      state.participant_id,
                      NULL
                  )
       );

-- Read cursors are harmless historical state, so a soft-deleted root may retain
-- its cursor. Orphan/non-root/cross-tenant cursors are still removed.
DELETE FROM thread_read_state AS state
 WHERE NOT EXISTS (
           SELECT 1
             FROM messages AS root
            WHERE root.id = state.root_message_id
              AND root.reply_to IS NULL
              AND aero_effective_room_access(
                      root.room_id,
                      state.participant_id,
                      NULL
                  )
       );

-- The early projection tables intentionally omitted subject foreign keys.
-- Backfill them now so hard deletes cannot leave unresolvable private state.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'message_drafts_participant_fk'
           AND conrelid = 'message_drafts'::regclass
    ) THEN
        ALTER TABLE message_drafts
            ADD CONSTRAINT message_drafts_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'message_drafts_room_fk'
           AND conrelid = 'message_drafts'::regclass
    ) THEN
        ALTER TABLE message_drafts
            ADD CONSTRAINT message_drafts_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_notification_prefs_participant_fk'
           AND conrelid = 'channel_notification_prefs'::regclass
    ) THEN
        ALTER TABLE channel_notification_prefs
            ADD CONSTRAINT channel_notification_prefs_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_notification_prefs_room_fk'
           AND conrelid = 'channel_notification_prefs'::regclass
    ) THEN
        ALTER TABLE channel_notification_prefs
            ADD CONSTRAINT channel_notification_prefs_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_favorites_participant_fk'
           AND conrelid = 'channel_favorites'::regclass
    ) THEN
        ALTER TABLE channel_favorites
            ADD CONSTRAINT channel_favorites_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_favorites_room_fk'
           AND conrelid = 'channel_favorites'::regclass
    ) THEN
        ALTER TABLE channel_favorites
            ADD CONSTRAINT channel_favorites_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_subscriptions_participant_fk'
           AND conrelid = 'thread_subscriptions'::regclass
    ) THEN
        ALTER TABLE thread_subscriptions
            ADD CONSTRAINT thread_subscriptions_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_subscriptions_root_fk'
           AND conrelid = 'thread_subscriptions'::regclass
    ) THEN
        ALTER TABLE thread_subscriptions
            ADD CONSTRAINT thread_subscriptions_root_fk
            FOREIGN KEY (root_message_id) REFERENCES messages(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_mutes_participant_fk'
           AND conrelid = 'thread_mutes'::regclass
    ) THEN
        ALTER TABLE thread_mutes
            ADD CONSTRAINT thread_mutes_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_mutes_root_fk'
           AND conrelid = 'thread_mutes'::regclass
    ) THEN
        ALTER TABLE thread_mutes
            ADD CONSTRAINT thread_mutes_root_fk
            FOREIGN KEY (root_message_id) REFERENCES messages(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_notification_prefs_participant_fk'
           AND conrelid = 'thread_notification_prefs'::regclass
    ) THEN
        ALTER TABLE thread_notification_prefs
            ADD CONSTRAINT thread_notification_prefs_participant_fk
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_notification_prefs_root_fk'
           AND conrelid = 'thread_notification_prefs'::regclass
    ) THEN
        ALTER TABLE thread_notification_prefs
            ADD CONSTRAINT thread_notification_prefs_root_fk
            FOREIGN KEY (root_message_id) REFERENCES messages(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'thread_read_state_root_fk'
           AND conrelid = 'thread_read_state'::regclass
    ) THEN
        ALTER TABLE thread_read_state
            ADD CONSTRAINT thread_read_state_root_fk
            FOREIGN KEY (root_message_id) REFERENCES messages(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
END
$$;

ALTER TABLE message_drafts
    VALIDATE CONSTRAINT message_drafts_participant_fk;
ALTER TABLE message_drafts
    VALIDATE CONSTRAINT message_drafts_room_fk;
ALTER TABLE channel_notification_prefs
    VALIDATE CONSTRAINT channel_notification_prefs_participant_fk;
ALTER TABLE channel_notification_prefs
    VALIDATE CONSTRAINT channel_notification_prefs_room_fk;
ALTER TABLE channel_favorites
    VALIDATE CONSTRAINT channel_favorites_participant_fk;
ALTER TABLE channel_favorites
    VALIDATE CONSTRAINT channel_favorites_room_fk;
ALTER TABLE thread_subscriptions
    VALIDATE CONSTRAINT thread_subscriptions_participant_fk;
ALTER TABLE thread_subscriptions
    VALIDATE CONSTRAINT thread_subscriptions_root_fk;
ALTER TABLE thread_mutes
    VALIDATE CONSTRAINT thread_mutes_participant_fk;
ALTER TABLE thread_mutes
    VALIDATE CONSTRAINT thread_mutes_root_fk;
ALTER TABLE thread_notification_prefs
    VALIDATE CONSTRAINT thread_notification_prefs_participant_fk;
ALTER TABLE thread_notification_prefs
    VALIDATE CONSTRAINT thread_notification_prefs_root_fk;
ALTER TABLE thread_read_state
    VALIDATE CONSTRAINT thread_read_state_root_fk;

CREATE OR REPLACE FUNCTION personal_room_state_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
       ) THEN
        RAISE EXCEPTION 'personal room-state identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_identity_immutable_chk';
    END IF;

    IF NOT aero_effective_room_access(
        NEW.room_id,
        NEW.participant_id,
        NULL
    ) THEN
        RAISE EXCEPTION 'personal room-state owner lacks effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_participant_scope_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS channel_mutes_scope_guard ON channel_mutes;
CREATE TRIGGER channel_mutes_scope_guard
    BEFORE INSERT OR UPDATE
    ON channel_mutes
    FOR EACH ROW
    EXECUTE FUNCTION personal_room_state_scope_guard();

DROP TRIGGER IF EXISTS channel_notification_prefs_scope_guard
    ON channel_notification_prefs;
CREATE TRIGGER channel_notification_prefs_scope_guard
    BEFORE INSERT OR UPDATE
    ON channel_notification_prefs
    FOR EACH ROW
    EXECUTE FUNCTION personal_room_state_scope_guard();

DROP TRIGGER IF EXISTS channel_favorites_scope_guard ON channel_favorites;
CREATE TRIGGER channel_favorites_scope_guard
    BEFORE INSERT OR UPDATE
    ON channel_favorites
    FOR EACH ROW
    EXECUTE FUNCTION personal_room_state_scope_guard();

CREATE OR REPLACE FUNCTION message_drafts_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    locked_room uuid;
    parent_deleted_at timestamptz;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
       ) THEN
        RAISE EXCEPTION 'draft owner/room identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'message_drafts_identity_immutable_chk';
    END IF;

    -- Permit only the system/FK cleanup of a reply edge whose old parent is no
    -- longer live. This must remain possible after owner deactivation, otherwise
    -- tombstoning the parent could be blocked by inaccessible private state.
    IF TG_OP = 'UPDATE'
       AND OLD.reply_to IS NOT NULL
       AND NEW.reply_to IS NULL
       AND NEW.blocks IS NOT DISTINCT FROM OLD.blocks
       AND NEW.updated_at IS NOT DISTINCT FROM OLD.updated_at
       AND NOT EXISTS (
           SELECT 1
             FROM messages AS parent
            WHERE parent.id = OLD.reply_to
              AND parent.room_id = OLD.room_id
              AND parent.deleted_at IS NULL
       ) THEN
        RETURN NEW;
    END IF;

    IF NOT aero_effective_room_access(
        NEW.room_id,
        NEW.participant_id,
        NULL
    ) THEN
        RAISE EXCEPTION 'draft owner lacks effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'message_drafts_participant_scope_chk';
    END IF;

    IF NEW.reply_to IS NOT NULL THEN
        SELECT room_id, deleted_at
          INTO locked_room, parent_deleted_at
          FROM messages
         WHERE id = NEW.reply_to
           FOR SHARE;
        IF NOT FOUND
           OR locked_room <> NEW.room_id
           OR parent_deleted_at IS NOT NULL THEN
            RAISE EXCEPTION 'draft reply target must be live and in its room'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'message_drafts_reply_live_scope_chk';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS message_drafts_scope_guard ON message_drafts;
CREATE TRIGGER message_drafts_scope_guard
    BEFORE INSERT OR UPDATE
    ON message_drafts
    FOR EACH ROW
    EXECUTE FUNCTION message_drafts_scope_guard();

CREATE OR REPLACE FUNCTION personal_thread_state_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_room uuid;
    locked_room uuid;
    root_deleted_at timestamptz;
    locked_reply_to uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.root_message_id IS DISTINCT FROM OLD.root_message_id
       ) THEN
        RAISE EXCEPTION 'personal thread-state identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_identity_immutable_chk';
    END IF;

    SELECT room_id
      INTO resolved_room
      FROM messages
     WHERE id = NEW.root_message_id
       AND reply_to IS NULL
       AND deleted_at IS NULL;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'personal thread state requires a live root message'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_root_scope_chk';
    END IF;

    IF NOT aero_effective_room_access(
        resolved_room,
        NEW.participant_id,
        NULL
    ) THEN
        RAISE EXCEPTION 'thread-state owner lacks effective root-room access'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_participant_scope_chk';
    END IF;

    SELECT room_id, deleted_at, reply_to
      INTO locked_room, root_deleted_at, locked_reply_to
      FROM messages
     WHERE id = NEW.root_message_id
       FOR SHARE;
    IF NOT FOUND
       OR locked_room <> resolved_room
       OR root_deleted_at IS NOT NULL
       OR locked_reply_to IS NOT NULL THEN
        RAISE EXCEPTION 'thread root is missing, moved, deleted, or a reply'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      TG_TABLE_NAME || '_root_scope_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS thread_subscriptions_scope_guard
    ON thread_subscriptions;
CREATE TRIGGER thread_subscriptions_scope_guard
    BEFORE INSERT OR UPDATE
    ON thread_subscriptions
    FOR EACH ROW
    EXECUTE FUNCTION personal_thread_state_scope_guard();

DROP TRIGGER IF EXISTS thread_mutes_scope_guard ON thread_mutes;
CREATE TRIGGER thread_mutes_scope_guard
    BEFORE INSERT OR UPDATE
    ON thread_mutes
    FOR EACH ROW
    EXECUTE FUNCTION personal_thread_state_scope_guard();

DROP TRIGGER IF EXISTS thread_notification_prefs_scope_guard
    ON thread_notification_prefs;
CREATE TRIGGER thread_notification_prefs_scope_guard
    BEFORE INSERT OR UPDATE
    ON thread_notification_prefs
    FOR EACH ROW
    EXECUTE FUNCTION personal_thread_state_scope_guard();

DROP TRIGGER IF EXISTS thread_read_state_scope_guard ON thread_read_state;
CREATE TRIGGER thread_read_state_scope_guard
    BEFORE INSERT OR UPDATE
    ON thread_read_state
    FOR EACH ROW
    EXECUTE FUNCTION personal_thread_state_scope_guard();

CREATE OR REPLACE FUNCTION dnd_settings_identity_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.participant_id IS DISTINCT FROM OLD.participant_id THEN
        RAISE EXCEPTION 'DND owner identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'dnd_settings_identity_immutable_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS dnd_settings_identity_guard ON dnd_settings;
CREATE TRIGGER dnd_settings_identity_guard
    BEFORE UPDATE OF participant_id
    ON dnd_settings
    FOR EACH ROW
    EXECUTE FUNCTION dnd_settings_identity_guard();

CREATE OR REPLACE FUNCTION purge_personal_state_on_room_leave()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    DELETE FROM message_drafts
     WHERE participant_id = OLD.participant_id
       AND room_id = OLD.room_id;
    DELETE FROM channel_mutes
     WHERE participant_id = OLD.participant_id
       AND room_id = OLD.room_id;
    DELETE FROM channel_notification_prefs
     WHERE participant_id = OLD.participant_id
       AND room_id = OLD.room_id;
    DELETE FROM channel_favorites
     WHERE participant_id = OLD.participant_id
       AND room_id = OLD.room_id;
    DELETE FROM thread_subscriptions AS state
     USING messages AS root
     WHERE state.participant_id = OLD.participant_id
       AND state.root_message_id = root.id
       AND root.room_id = OLD.room_id;
    DELETE FROM thread_mutes AS state
     USING messages AS root
     WHERE state.participant_id = OLD.participant_id
       AND state.root_message_id = root.id
       AND root.room_id = OLD.room_id;
    DELETE FROM thread_notification_prefs AS state
     USING messages AS root
     WHERE state.participant_id = OLD.participant_id
       AND state.root_message_id = root.id
       AND root.room_id = OLD.room_id;
    DELETE FROM thread_read_state AS state
     USING messages AS root
     WHERE state.participant_id = OLD.participant_id
       AND state.root_message_id = root.id
       AND root.room_id = OLD.room_id;
    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS purge_personal_state_on_room_leave ON room_members;
CREATE TRIGGER purge_personal_state_on_room_leave
    AFTER DELETE
    ON room_members
    FOR EACH ROW
    EXECUTE FUNCTION purge_personal_state_on_room_leave();

CREATE OR REPLACE FUNCTION purge_personal_thread_state_on_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE message_drafts
       SET reply_to = NULL
     WHERE reply_to = NEW.id;
    DELETE FROM thread_subscriptions
     WHERE root_message_id = NEW.id;
    DELETE FROM thread_mutes
     WHERE root_message_id = NEW.id;
    DELETE FROM thread_notification_prefs
     WHERE root_message_id = NEW.id;
    -- Preserve thread_read_state as a historical cursor. New writes remain
    -- forbidden by the live-root trigger, and hard deletion cascades it.
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS purge_personal_thread_state_on_tombstone ON messages;
CREATE TRIGGER purge_personal_thread_state_on_tombstone
    AFTER UPDATE OF deleted_at
    ON messages
    FOR EACH ROW
    WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION purge_personal_thread_state_on_tombstone();

COMMENT ON FUNCTION personal_room_state_scope_guard() IS
    'Backstops immutable owner/room identity and current effective room access for private room state.';
COMMENT ON FUNCTION personal_thread_state_scope_guard() IS
    'Backstops immutable owner/root identity, live canonical roots, and current effective root-room access.';
