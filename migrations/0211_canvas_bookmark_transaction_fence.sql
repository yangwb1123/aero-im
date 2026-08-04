-- Transaction and tenant fences for channel canvases, collaborative operations,
-- and channel bookmarks.
--
-- Canonical application paths now hold effective workspace/room/membership
-- locks through every read or mutation. These constraints and triggers protect
-- raw SQL/old writers, freeze retained resource identity, and make a canvas op
-- plus its realtime RoomEvent one PostgreSQL commit.

-- These early feature tables predated room foreign keys and channel-only
-- enforcement. Remove orphan/non-channel projections before tightening them.
DELETE FROM channel_canvases canvas
 WHERE NOT EXISTS (
           SELECT 1
             FROM rooms room
            WHERE room.id = canvas.room_id
              AND room.kind = 'channel'
       );

DELETE FROM channel_bookmarks bookmark
 WHERE NOT EXISTS (
           SELECT 1
             FROM rooms room
            WHERE room.id = bookmark.room_id
              AND room.kind = 'channel'
       );

-- Canonicalize permissive historical values before installing the same bounds
-- enforced by storage and HTTP. Preserve operation sequence positions by
-- replacing an unusable legacy payload rather than deleting it.
DELETE FROM channel_canvases WHERE btrim(title) = '';
UPDATE channel_canvases
   SET title = left(btrim(title), 512)
 WHERE title IS DISTINCT FROM left(btrim(title), 512);
UPDATE channel_canvases
   SET blocks = '[]'::jsonb
 WHERE jsonb_typeof(blocks) <> 'array'
    OR octet_length(blocks::text) > 1048576;

DELETE FROM channel_bookmarks
 WHERE btrim(title) = '' OR btrim(url) = '';
UPDATE channel_bookmarks
   SET title = left(btrim(title), 256),
       url = left(btrim(url), 2048),
       emoji = NULLIF(left(btrim(emoji), 64), '')
 WHERE title IS DISTINCT FROM left(btrim(title), 256)
    OR url IS DISTINCT FROM left(btrim(url), 2048)
    OR emoji IS DISTINCT FROM NULLIF(left(btrim(emoji), 64), '');

UPDATE canvas_ops
   SET op = '{"type":"legacy_unsupported"}'::jsonb
 WHERE jsonb_typeof(op) <> 'object'
    OR octet_length(op::text) > 65536;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_canvases_room_fk'
           AND conrelid = 'channel_canvases'::regclass
    ) THEN
        ALTER TABLE channel_canvases
            ADD CONSTRAINT channel_canvases_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_bookmarks_room_fk'
           AND conrelid = 'channel_bookmarks'::regclass
    ) THEN
        ALTER TABLE channel_bookmarks
            ADD CONSTRAINT channel_bookmarks_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id)
            ON DELETE CASCADE NOT VALID;
    END IF;
END
$$;

ALTER TABLE channel_canvases
    VALIDATE CONSTRAINT channel_canvases_room_fk;
ALTER TABLE channel_bookmarks
    VALIDATE CONSTRAINT channel_bookmarks_room_fk;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_canvases_content_chk'
           AND conrelid = 'channel_canvases'::regclass
    ) THEN
        ALTER TABLE channel_canvases
            ADD CONSTRAINT channel_canvases_content_chk
            CHECK (
                title = btrim(title)
                AND char_length(title) BETWEEN 1 AND 512
                AND jsonb_typeof(blocks) = 'array'
                AND octet_length(blocks::text) <= 1048576
                AND version >= 0
                AND op_seq >= 0
            );
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_bookmarks_content_chk'
           AND conrelid = 'channel_bookmarks'::regclass
    ) THEN
        ALTER TABLE channel_bookmarks
            ADD CONSTRAINT channel_bookmarks_content_chk
            CHECK (
                title = btrim(title)
                AND char_length(title) BETWEEN 1 AND 256
                AND url = btrim(url)
                AND char_length(url) BETWEEN 1 AND 2048
                AND (
                    emoji IS NULL
                    OR (
                        emoji = btrim(emoji)
                        AND char_length(emoji) BETWEEN 1 AND 64
                    )
                )
            );
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'canvas_ops_content_chk'
           AND conrelid = 'canvas_ops'::regclass
    ) THEN
        ALTER TABLE canvas_ops
            ADD CONSTRAINT canvas_ops_content_chk
            CHECK (
                seq > 0
                AND jsonb_typeof(op) = 'object'
                AND octet_length(op::text) <= 65536
            );
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION channel_canvas_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_workspace uuid;
    locked_kind text;
    locked_archived boolean;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.author_id IS DISTINCT FROM OLD.author_id
           OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
            RAISE EXCEPTION 'channel canvas room/author identity is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_canvases_identity_immutable_chk';
        END IF;
        IF NEW.version < OLD.version
           OR NEW.op_seq < OLD.op_seq
           OR NEW.snapshot_op_seq < OLD.snapshot_op_seq
           OR NEW.updated_at < OLD.updated_at THEN
            RAISE EXCEPTION 'channel canvas versions and cursors are monotonic'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_canvases_monotonic_chk';
        END IF;
    END IF;

    -- Resolve without a row lock, then acquire the platform lock order. Inserts
    -- additionally prove the immutable author is an effective room member.
    SELECT workspace_id
      INTO resolved_workspace
      FROM rooms
     WHERE id = NEW.room_id
       AND kind = 'channel'
       AND NOT is_archived;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'channel canvas requires a live channel'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_canvases_room_scope_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF NOT aero_effective_room_access(
            NEW.room_id,
            NEW.author_id,
            resolved_workspace
        ) THEN
            RAISE EXCEPTION 'channel canvas author lacks effective channel access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_canvases_author_scope_chk';
        END IF;
    ELSE
        PERFORM 1
          FROM workspaces
         WHERE id = resolved_workspace
           FOR SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'channel canvas workspace disappeared'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_canvases_room_scope_chk';
        END IF;
    END IF;

    SELECT kind, is_archived
      INTO locked_kind, locked_archived
      FROM rooms
     WHERE id = NEW.room_id
       AND workspace_id = resolved_workspace
       FOR SHARE;
    IF NOT FOUND OR locked_kind <> 'channel' OR locked_archived THEN
        RAISE EXCEPTION 'channel canvas room changed or is not live'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_canvases_room_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS channel_canvas_scope_guard ON channel_canvases;
CREATE TRIGGER channel_canvas_scope_guard
    BEFORE INSERT OR UPDATE
    ON channel_canvases
    FOR EACH ROW
    EXECUTE FUNCTION channel_canvas_scope_guard();

COMMENT ON TRIGGER channel_canvas_scope_guard ON channel_canvases IS
    'Freezes room/author identity, requires a live channel, and checks effective author access only at creation so historical rows survive membership departure.';

CREATE OR REPLACE FUNCTION canvas_op_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_room uuid;
    resolved_workspace uuid;
    locked_room uuid;
    locked_op_seq bigint;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.canvas_id IS DISTINCT FROM OLD.canvas_id
           OR NEW.client_op_id IS DISTINCT FROM OLD.client_op_id
           OR NEW.seq IS DISTINCT FROM OLD.seq
           OR NEW.author_id IS DISTINCT FROM OLD.author_id
           OR NEW.op IS DISTINCT FROM OLD.op
           OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
            RAISE EXCEPTION 'canvas operation is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'canvas_ops_identity_immutable_chk';
        END IF;
        RETURN NEW;
    END IF;

    SELECT canvas.room_id, room.workspace_id
      INTO resolved_room, resolved_workspace
      FROM channel_canvases canvas
      JOIN rooms room ON room.id = canvas.room_id
     WHERE canvas.id = NEW.canvas_id
       AND room.kind = 'channel'
       AND NOT room.is_archived;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'canvas operation requires a canvas in a live channel'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'canvas_ops_canvas_scope_chk';
    END IF;

    IF NOT aero_effective_room_access(
        resolved_room,
        NEW.author_id,
        resolved_workspace
    ) THEN
        RAISE EXCEPTION 'canvas operation author lacks effective channel access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'canvas_ops_author_scope_chk';
    END IF;

    SELECT room_id, op_seq
      INTO locked_room, locked_op_seq
      FROM channel_canvases
     WHERE id = NEW.canvas_id
       FOR SHARE;
    IF NOT FOUND
       OR locked_room <> resolved_room
       OR locked_op_seq <> NEW.seq THEN
        RAISE EXCEPTION 'canvas operation canvas/sequence identity mismatch'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'canvas_ops_canvas_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS canvas_ops_scope_guard ON canvas_ops;
CREATE TRIGGER canvas_ops_scope_guard
    BEFORE INSERT OR UPDATE
    ON canvas_ops
    FOR EACH ROW
    EXECUTE FUNCTION canvas_op_scope_guard();

COMMENT ON TRIGGER canvas_ops_scope_guard ON canvas_ops IS
    'Backstops immutable op identity, current effective author access, live-channel containment, and the canvas sequence allocator.';

CREATE OR REPLACE FUNCTION channel_bookmark_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_workspace uuid;
    locked_kind text;
    locked_archived boolean;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.created_by IS DISTINCT FROM OLD.created_by
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
       ) THEN
        RAISE EXCEPTION 'channel bookmark room/creator identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_bookmarks_identity_immutable_chk';
    END IF;

    SELECT workspace_id
      INTO resolved_workspace
      FROM rooms
     WHERE id = NEW.room_id
       AND kind = 'channel'
       AND NOT is_archived;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'channel bookmark requires a live channel'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_bookmarks_room_scope_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF NOT aero_effective_room_access(
            NEW.room_id,
            NEW.created_by,
            resolved_workspace
        ) THEN
            RAISE EXCEPTION 'channel bookmark creator lacks effective channel access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_bookmarks_creator_scope_chk';
        END IF;
    ELSE
        PERFORM 1
          FROM workspaces
         WHERE id = resolved_workspace
           FOR SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'channel bookmark workspace disappeared'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'channel_bookmarks_room_scope_chk';
        END IF;
    END IF;

    SELECT kind, is_archived
      INTO locked_kind, locked_archived
      FROM rooms
     WHERE id = NEW.room_id
       AND workspace_id = resolved_workspace
       FOR SHARE;
    IF NOT FOUND OR locked_kind <> 'channel' OR locked_archived THEN
        RAISE EXCEPTION 'channel bookmark room changed or is not live'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_bookmarks_room_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS channel_bookmark_scope_guard ON channel_bookmarks;
CREATE TRIGGER channel_bookmark_scope_guard
    BEFORE INSERT OR UPDATE
    ON channel_bookmarks
    FOR EACH ROW
    EXECUTE FUNCTION channel_bookmark_scope_guard();

COMMENT ON TRIGGER channel_bookmark_scope_guard ON channel_bookmarks IS
    'Freezes room/creator identity, requires a live channel, and preserves historical bookmarks after their creator leaves.';

-- Canvas operations reuse the durable per-subject room-event queue. The legacy
-- column name `message_id` stores the immutable op UUID for this event kind.
ALTER TABLE event_outbox
    DROP CONSTRAINT IF EXISTS event_outbox_kind_check;

ALTER TABLE event_outbox
    ADD CONSTRAINT event_outbox_kind_check
        CHECK (
            event_kind IN (
                'message',
                'edited',
                'deleted',
                'notify',
                'reaction',
                'canvas_op'
            )
        );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'event_outbox_canvas_op_payload_chk'
           AND conrelid = 'event_outbox'::regclass
    ) THEN
        ALTER TABLE event_outbox
            ADD CONSTRAINT event_outbox_canvas_op_payload_chk
            CHECK (
                event_kind <> 'canvas_op'
                OR (
                    event_id = message_id
                    AND aggregate_version = 1
                    AND payload->>'kind' = 'canvas_op'
                    AND payload->>'op_id' = message_id::text
                    AND payload->>'op_seq' ~ '^[1-9][0-9]*$'
                    AND jsonb_typeof(payload->'op') = 'object'
                    AND subject = 'im.room.' || (payload->>'room_id')
                )
            );
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION canvas_op_outbox_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    op_room uuid;
    op_canvas uuid;
    op_seq bigint;
    op_author uuid;
    op_payload jsonb;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           OLD.event_kind = 'canvas_op'
           OR NEW.event_kind = 'canvas_op'
       )
       AND (
           NEW.event_id IS DISTINCT FROM OLD.event_id
           OR NEW.message_id IS DISTINCT FROM OLD.message_id
           OR NEW.event_kind IS DISTINCT FROM OLD.event_kind
           OR NEW.subject IS DISTINCT FROM OLD.subject
           OR NEW.payload IS DISTINCT FROM OLD.payload
       ) THEN
        RAISE EXCEPTION 'canvas operation outbox identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'event_outbox_canvas_op_identity_immutable_chk';
    END IF;

    IF NEW.event_kind <> 'canvas_op' THEN
        RETURN NEW;
    END IF;

    SELECT canvas.room_id, operation.canvas_id, operation.seq,
           operation.author_id, operation.op
      INTO op_room, op_canvas, op_seq, op_author, op_payload
      FROM canvas_ops operation
      JOIN channel_canvases canvas ON canvas.id = operation.canvas_id
     WHERE operation.id = NEW.message_id
       FOR KEY SHARE OF operation, canvas;
    IF NOT FOUND
       OR NEW.event_id <> NEW.message_id
       OR NEW.payload->>'kind' IS DISTINCT FROM 'canvas_op'
       OR NEW.payload->>'op_id' IS DISTINCT FROM NEW.message_id::text
       OR NEW.payload->>'room_id' IS DISTINCT FROM aero_uuid_to_ulid(op_room)
       OR NEW.payload->>'canvas_id' IS DISTINCT FROM aero_uuid_to_ulid(op_canvas)
       OR NEW.payload->>'op_seq' IS DISTINCT FROM op_seq::text
       OR NEW.payload->>'author_id' IS DISTINCT FROM aero_uuid_to_ulid(op_author)
       OR NEW.payload->'op' IS DISTINCT FROM op_payload
       OR NEW.subject IS DISTINCT FROM
          ('im.room.' || aero_uuid_to_ulid(op_room)) THEN
        RAISE EXCEPTION 'canvas operation outbox identity mismatch'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'event_outbox_canvas_op_scope_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS canvas_op_outbox_scope_guard ON event_outbox;
CREATE TRIGGER canvas_op_outbox_scope_guard
    BEFORE INSERT OR UPDATE OF
        event_id, message_id, event_kind, subject, payload
    ON event_outbox
    FOR EACH ROW
    EXECUTE FUNCTION canvas_op_outbox_scope_guard();

COMMENT ON TRIGGER canvas_op_outbox_scope_guard ON event_outbox IS
    'Backstops canonical op/canvas/room/author/payload containment for durable canvas realtime events.';

CREATE OR REPLACE FUNCTION canvas_op_requires_durable_outbox()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Skip an op that was itself removed by a same-transaction canvas/room
    -- delete. Every operation that survives commit must have its stable event.
    IF EXISTS (SELECT 1 FROM canvas_ops WHERE id = NEW.id)
       AND NOT EXISTS (
           SELECT 1
             FROM event_outbox
            WHERE event_kind = 'canvas_op'
              AND event_id = NEW.id
              AND message_id = NEW.id
       ) THEN
        RAISE EXCEPTION 'canvas operation committed without durable realtime outbox'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'canvas_ops_durable_outbox_chk';
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS canvas_op_requires_durable_outbox ON canvas_ops;
CREATE CONSTRAINT TRIGGER canvas_op_requires_durable_outbox
    AFTER INSERT
    ON canvas_ops
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION canvas_op_requires_durable_outbox();

COMMENT ON TRIGGER canvas_op_requires_durable_outbox ON canvas_ops IS
    'Commit-time atomicity fence: every surviving canvas op must have its stable canvas_op event_outbox row.';
