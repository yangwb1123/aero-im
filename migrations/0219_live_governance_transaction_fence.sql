-- Transaction fences and raw-SQL backstops for live-stream governance.
--
-- Application writers authorize room-linked actors before locking streams,
-- then serialize every moderation mutation on the canonical stream row.
-- Moderator revocation therefore has one deterministic before/after order with
-- an in-flight ban. Raids additionally lock the actor identity and both streams
-- in UUID order, making the per-raider cooldown linearizable.
--
-- The transaction-local actor setting is not an authentication replacement;
-- it prevents future/raw writers from persisting another participant as the
-- audit actor by accident. Triggers also independently recheck the canonical
-- owner/moderator relationship and active identities.

CREATE OR REPLACE FUNCTION aero_live_governance_actor()
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    SELECT NULLIF(
        current_setting('aero.live_governance_actor', true),
        ''
    )::uuid
$$;

CREATE OR REPLACE FUNCTION stream_moderator_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    request_actor uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.id IS DISTINCT FROM OLD.id
           OR NEW.stream_id IS DISTINCT FROM OLD.stream_id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.created_by IS DISTINCT FROM OLD.created_by
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
       ) THEN
        RAISE EXCEPTION 'stream moderator assignment identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_identity_immutable_chk';
    END IF;

    request_actor := aero_live_governance_actor();
    IF request_actor IS NULL OR request_actor <> NEW.created_by THEN
        RAISE EXCEPTION 'stream moderator audit actor is not authenticated by the writer'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_actor_context_chk';
    END IF;

    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = NEW.stream_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'stream moderator requires a canonical stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_stream_scope_chk';
    END IF;
    IF NEW.created_by <> canonical_owner THEN
        RAISE EXCEPTION 'only the canonical stream owner may grant moderator authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_owner_identity_chk';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = NEW.participant_id
           AND participant.deleted_at IS NULL
    ) THEN
        RAISE EXCEPTION 'stream moderator target must be an active participant'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_target_identity_chk';
    END IF;

    IF canonical_room IS NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM participants AS participant
             WHERE participant.id = NEW.created_by
               AND participant.deleted_at IS NULL
        ) THEN
            RAISE EXCEPTION 'stream moderator actor must be active'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_moderators_actor_identity_chk';
        END IF;
    ELSE
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               NEW.created_by
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = NEW.created_by
           ) THEN
            RAISE EXCEPTION 'stream moderator actor lacks effective canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_moderators_actor_room_scope_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS stream_moderator_transaction_fence
    ON stream_moderators;
CREATE TRIGGER stream_moderator_transaction_fence
    BEFORE INSERT OR UPDATE
    ON stream_moderators
    FOR EACH ROW
    EXECUTE FUNCTION stream_moderator_transaction_fence();

CREATE OR REPLACE FUNCTION stream_ban_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    request_actor uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.stream_id IS DISTINCT FROM OLD.stream_id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
       ) THEN
        RAISE EXCEPTION 'stream ban aggregate identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_identity_immutable_chk';
    END IF;

    request_actor := aero_live_governance_actor();
    IF NEW.banned_by IS NULL
       OR request_actor IS NULL
       OR request_actor <> NEW.banned_by THEN
        RAISE EXCEPTION 'stream ban audit actor is not authenticated by the writer'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_actor_context_chk';
    END IF;

    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = NEW.stream_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'stream ban requires a canonical stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_stream_scope_chk';
    END IF;
    IF NEW.banned_by <> canonical_owner
       AND NOT EXISTS (
           SELECT 1
             FROM stream_moderators AS moderator
            WHERE moderator.stream_id = NEW.stream_id
              AND moderator.participant_id = NEW.banned_by
       ) THEN
        RAISE EXCEPTION 'stream ban actor lacks current owner/moderator authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_actor_authority_chk';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = NEW.participant_id
           AND participant.deleted_at IS NULL
    ) THEN
        RAISE EXCEPTION 'stream ban target must be an active participant'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_target_identity_chk';
    END IF;

    IF canonical_room IS NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM participants AS participant
             WHERE participant.id = NEW.banned_by
               AND participant.deleted_at IS NULL
        ) THEN
            RAISE EXCEPTION 'stream ban actor must be active'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_bans_actor_identity_chk';
        END IF;
    ELSE
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               NEW.banned_by
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = NEW.banned_by
           ) THEN
            RAISE EXCEPTION 'stream ban actor lacks effective canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_bans_actor_room_scope_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS stream_ban_transaction_fence
    ON stream_bans;
CREATE TRIGGER stream_ban_transaction_fence
    BEFORE INSERT OR UPDATE
    ON stream_bans
    FOR EACH ROW
    EXECUTE FUNCTION stream_ban_transaction_fence();

CREATE OR REPLACE FUNCTION raid_history_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    request_actor uuid;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'raid history is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_immutable_chk';
    END IF;

    request_actor := aero_live_governance_actor();
    IF request_actor IS NULL OR request_actor <> NEW.raider_id THEN
        RAISE EXCEPTION 'raid audit actor is not authenticated by the writer'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_actor_context_chk';
    END IF;
    IF NEW.source_stream = NEW.target_stream THEN
        RAISE EXCEPTION 'raid source and target must differ'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_distinct_streams_chk';
    END IF;

    -- Match the repository's per-raider fence before taking/upgrading identity
    -- row locks. This also prevents two room-linked raids from both holding
    -- participant SHARE and deadlocking on UPDATE upgrades.
    PERFORM pg_advisory_xact_lock(
        hashtextextended(NEW.raider_id::text, 219)
    );

    -- This identity lock fences account erasure and is re-entrant for supported
    -- repository writers.
    PERFORM 1
      FROM participants AS participant
     WHERE participant.id = NEW.raider_id
       AND participant.deleted_at IS NULL
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'raid actor must be an active participant'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_actor_identity_chk';
    END IF;

    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = NEW.source_stream;
    IF NOT FOUND OR canonical_owner <> NEW.raider_id THEN
        RAISE EXCEPTION 'raid actor must own the canonical source stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_source_owner_chk';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM streams AS target WHERE target.id = NEW.target_stream
    ) THEN
        RAISE EXCEPTION 'raid target must be a canonical stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_target_scope_chk';
    END IF;

    IF canonical_room IS NOT NULL THEN
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               NEW.raider_id
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = NEW.raider_id
           ) THEN
            RAISE EXCEPTION 'raid actor lacks effective canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'raid_history_actor_room_scope_chk';
        END IF;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM raid_history AS prior
         WHERE prior.raider_id = NEW.raider_id
           AND prior.created_at > NEW.created_at - INTERVAL '60 seconds'
           AND prior.created_at < NEW.created_at + INTERVAL '60 seconds'
    ) THEN
        RAISE EXCEPTION 'raid cooldown active'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'raid_history_cooldown_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS raid_history_transaction_fence
    ON raid_history;
CREATE TRIGGER raid_history_transaction_fence
    BEFORE INSERT OR UPDATE
    ON raid_history
    FOR EACH ROW
    EXECUTE FUNCTION raid_history_transaction_fence();

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'stream_bans_reason_length_chk'
           AND conrelid = 'stream_bans'::regclass
    ) THEN
        ALTER TABLE stream_bans
            ADD CONSTRAINT stream_bans_reason_length_chk
            CHECK (reason IS NULL OR char_length(reason) <= 500)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'raid_history_distinct_streams_chk'
           AND conrelid = 'raid_history'::regclass
    ) THEN
        ALTER TABLE raid_history
            ADD CONSTRAINT raid_history_distinct_streams_chk
            CHECK (source_stream <> target_stream)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'raid_history_nonnegative_viewers_chk'
           AND conrelid = 'raid_history'::regclass
    ) THEN
        ALTER TABLE raid_history
            ADD CONSTRAINT raid_history_nonnegative_viewers_chk
            CHECK (viewer_count >= 0)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'raid_history_message_length_chk'
           AND conrelid = 'raid_history'::regclass
    ) THEN
        ALTER TABLE raid_history
            ADD CONSTRAINT raid_history_message_length_chk
            CHECK (message IS NULL OR char_length(message) <= 500)
            NOT VALID;
    END IF;
END
$$;

COMMENT ON FUNCTION aero_live_governance_actor() IS
    'Transaction-local audit actor supplied by actor-aware live-governance repositories';
COMMENT ON TRIGGER stream_moderator_transaction_fence ON stream_moderators IS
    'Raw-SQL backstop for canonical stream owner grants, active targets, immutable role identity, and authenticated audit actor';
COMMENT ON TRIGGER stream_ban_transaction_fence ON stream_bans IS
    'Raw-SQL backstop for canonical owner/mod authority, active target identity, immutable scope, and authenticated audit actor';
COMMENT ON TRIGGER raid_history_transaction_fence ON raid_history IS
    'Raw-SQL backstop for immutable canonical raids, current effective source ownership, target existence, authenticated actor, and serialized cooldown';
