-- Bind each appeal to one concrete ban incarnation and keep 0219 governance
-- writers rolling-compatible.
--
-- `stream_bans` is keyed by (stream, participant), so an unban followed by a
-- re-ban reuses the same aggregate key. A monotonic revision prevents an appeal
-- submitted for the old row from deleting the replacement ban. The revision
-- trigger also covers pre-0225 INSERT/ON CONFLICT writers: every successful
-- insert or update receives a fresh, server-owned revision.

CREATE SEQUENCE IF NOT EXISTS stream_ban_revision_seq AS bigint;

ALTER TABLE stream_bans
    ADD COLUMN IF NOT EXISTS ban_revision bigint
        NOT NULL DEFAULT nextval('stream_ban_revision_seq');

ALTER SEQUENCE stream_ban_revision_seq
    OWNED BY stream_bans.ban_revision;

CREATE UNIQUE INDEX IF NOT EXISTS stream_bans_revision_uidx
    ON stream_bans (ban_revision);

ALTER TABLE ban_appeals
    ADD COLUMN IF NOT EXISTS ban_revision bigint;

-- Bind at most one legacy pending appeal to the currently active ban. Extra
-- legacy duplicates remain historical/unbound and can be reviewed, but can
-- never lift a later ban.
WITH ranked AS (
    SELECT appeal.id,
           ban.ban_revision,
           row_number() OVER (
               PARTITION BY appeal.stream_id, appeal.appellant_id
               ORDER BY appeal.created_at, appeal.id
           ) AS ordinal
      FROM ban_appeals AS appeal
      JOIN stream_bans AS ban
        ON ban.stream_id = appeal.stream_id
       AND ban.participant_id = appeal.appellant_id
     WHERE appeal.status = 'pending'
       AND (ban.until IS NULL OR ban.until > clock_timestamp())
       AND appeal.ban_revision IS NULL
)
UPDATE ban_appeals AS appeal
   SET ban_revision = ranked.ban_revision
  FROM ranked
 WHERE appeal.id = ranked.id
   AND ranked.ordinal = 1;

CREATE UNIQUE INDEX IF NOT EXISTS ban_appeals_one_pending_revision_uidx
    ON ban_appeals (stream_id, appellant_id, ban_revision)
    WHERE status = 'pending' AND ban_revision IS NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'ban_appeals_reason_length_chk'
           AND conrelid = 'ban_appeals'::regclass
    ) THEN
        ALTER TABLE ban_appeals
            ADD CONSTRAINT ban_appeals_reason_length_chk
            CHECK (
                char_length(btrim(appeal_reason)) BETWEEN 1 AND 2000
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'ban_appeals_decision_reason_length_chk'
           AND conrelid = 'ban_appeals'::regclass
    ) THEN
        ALTER TABLE ban_appeals
            ADD CONSTRAINT ban_appeals_decision_reason_length_chk
            CHECK (
                decision_reason IS NULL
                OR char_length(decision_reason) <= 2000
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'ban_appeals_review_state_chk'
           AND conrelid = 'ban_appeals'::regclass
    ) THEN
        ALTER TABLE ban_appeals
            ADD CONSTRAINT ban_appeals_review_state_chk
            CHECK (
                (
                    status = 'pending'
                    AND reviewed_by IS NULL
                    AND reviewed_at IS NULL
                    AND decision_reason IS NULL
                )
                OR (
                    status IN ('approved', 'denied')
                    AND reviewed_by IS NOT NULL
                    AND reviewed_at IS NOT NULL
                )
            )
            NOT VALID;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION live_governance_ban_revision_compatibility()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.ban_revision := nextval('stream_ban_revision_seq');
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS live_governance_ban_revision_compatibility
    ON stream_bans;
CREATE TRIGGER live_governance_ban_revision_compatibility
    BEFORE INSERT OR UPDATE
    ON stream_bans
    FOR EACH ROW
    EXECUTE FUNCTION live_governance_ban_revision_compatibility();

CREATE OR REPLACE FUNCTION live_governance_raid_compatibility()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Never trust a caller-supplied timestamp for the 60-second cooldown.
    NEW.created_at := clock_timestamp();
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS live_governance_raid_compatibility
    ON raid_history;
CREATE TRIGGER live_governance_raid_compatibility
    BEFORE INSERT
    ON raid_history
    FOR EACH ROW
    EXECUTE FUNCTION live_governance_raid_compatibility();

CREATE OR REPLACE FUNCTION stream_moderator_delete_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    request_actor uuid;
BEGIN
    -- A hard participant delete owns its FK cascade. Tombstones still exist and
    -- therefore use the ordinary owner-authorized path.
    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = OLD.participant_id
    ) THEN
        RETURN OLD;
    END IF;

    request_actor := aero_live_governance_actor();
    IF request_actor IS NULL THEN
        RAISE EXCEPTION 'stream moderator removal requires actor context'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_delete_actor_context_chk';
    END IF;

    -- DELETE has already locked the assignment row. NOWAIT prevents a raw
    -- writer from inverting the supported stream -> assignment lock order.
    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = OLD.stream_id
       FOR UPDATE NOWAIT;
    IF NOT FOUND OR request_actor <> canonical_owner THEN
        RAISE EXCEPTION 'only the current stream owner may remove a moderator'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_moderators_delete_owner_chk';
    END IF;

    IF canonical_room IS NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM participants AS participant
             WHERE participant.id = request_actor
               AND participant.deleted_at IS NULL
        ) THEN
            RAISE EXCEPTION 'stream moderator removal actor must be active'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'stream_moderators_delete_actor_identity_chk';
        END IF;
    ELSE
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               request_actor
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = request_actor
           ) THEN
            RAISE EXCEPTION
                'stream moderator removal actor lacks canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'stream_moderators_delete_actor_room_scope_chk';
        END IF;
    END IF;

    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS stream_moderator_delete_fence
    ON stream_moderators;
CREATE TRIGGER stream_moderator_delete_fence
    BEFORE DELETE
    ON stream_moderators
    FOR EACH ROW
    EXECUTE FUNCTION stream_moderator_delete_fence();

CREATE OR REPLACE FUNCTION stream_ban_delete_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    request_actor uuid;
BEGIN
    -- Expiry cleanup is an actorless system sweep, not an active unban.
    IF OLD.until IS NOT NULL AND OLD.until <= clock_timestamp() THEN
        RETURN OLD;
    END IF;

    -- Permit the participant FK's hard-delete cascade. A tombstoned target
    -- remains present and does not receive this exemption.
    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = OLD.participant_id
    ) THEN
        RETURN OLD;
    END IF;

    request_actor := aero_live_governance_actor();
    IF request_actor IS NULL THEN
        RAISE EXCEPTION 'active stream unban requires actor context'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_delete_actor_context_chk';
    END IF;

    -- DELETE has already locked the ban row. NOWAIT prevents raw SQL from
    -- deadlocking with supported stream -> ban writers.
    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = OLD.stream_id
       FOR UPDATE NOWAIT;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'active stream unban requires a canonical stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_delete_stream_scope_chk';
    END IF;

    IF request_actor <> canonical_owner
       AND NOT EXISTS (
           SELECT 1
             FROM stream_moderators AS moderator
            WHERE moderator.stream_id = OLD.stream_id
              AND moderator.participant_id = request_actor
       ) THEN
        RAISE EXCEPTION 'stream unban actor lacks current authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'stream_bans_delete_actor_authority_chk';
    END IF;

    IF canonical_room IS NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM participants AS participant
             WHERE participant.id = request_actor
               AND participant.deleted_at IS NULL
        ) THEN
            RAISE EXCEPTION 'stream unban actor must be active'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_bans_delete_actor_identity_chk';
        END IF;
    ELSE
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               request_actor
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = request_actor
           ) THEN
            RAISE EXCEPTION 'stream unban actor lacks canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'stream_bans_delete_actor_room_scope_chk';
        END IF;
    END IF;

    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS stream_ban_delete_fence
    ON stream_bans;
CREATE TRIGGER stream_ban_delete_fence
    BEFORE DELETE
    ON stream_bans
    FOR EACH ROW
    EXECUTE FUNCTION stream_ban_delete_fence();

CREATE OR REPLACE FUNCTION raid_history_delete_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'raid history is immutable'
        USING ERRCODE = '23514',
              CONSTRAINT = 'raid_history_immutable_chk';
END
$$;

DROP TRIGGER IF EXISTS raid_history_delete_fence
    ON raid_history;
CREATE TRIGGER raid_history_delete_fence
    BEFORE DELETE
    ON raid_history
    FOR EACH ROW
    EXECUTE FUNCTION raid_history_delete_fence();

CREATE OR REPLACE FUNCTION ban_appeal_transaction_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_revision bigint;
    canonical_owner uuid;
    canonical_room uuid;
    canonical_workspace uuid;
    locked_owner uuid;
    locked_room uuid;
    request_actor uuid;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'pending'
           OR NEW.reviewed_by IS NOT NULL
           OR NEW.reviewed_at IS NOT NULL
           OR NEW.decision_reason IS NOT NULL THEN
            RAISE EXCEPTION 'new ban appeal must be pending and undecided'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_initial_state_chk';
        END IF;

        NEW.appeal_reason := btrim(NEW.appeal_reason);
        NEW.created_at := clock_timestamp();
        IF NEW.appeal_reason = ''
           OR char_length(NEW.appeal_reason) > 2000 THEN
            RAISE EXCEPTION 'invalid ban appeal reason'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_reason_length_chk';
        END IF;

        request_actor := aero_live_governance_actor();
        IF request_actor IS NULL OR request_actor <> NEW.appellant_id THEN
            RAISE EXCEPTION 'ban appeal actor does not match appellant'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_actor_context_chk';
        END IF;

        -- Resolve without a stream lock, then enter the same global scope order
        -- as application writers before taking the stream and current-ban rows.
        SELECT stream.owner_id, stream.room_id
          INTO canonical_owner, canonical_room
          FROM streams AS stream
         WHERE stream.id = NEW.stream_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'ban appeal requires a canonical stream'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_stream_scope_chk';
        END IF;

        IF canonical_room IS NULL THEN
            PERFORM 1
              FROM participants AS participant
             WHERE participant.id = NEW.appellant_id
               AND participant.deleted_at IS NULL
               FOR SHARE;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'ban appeal appellant must be active'
                    USING ERRCODE = '23514',
                          CONSTRAINT =
                              'ban_appeals_appellant_identity_chk';
            END IF;
        ELSIF NOT aero_effective_room_access(
            canonical_room,
            NEW.appellant_id,
            NULL
        ) THEN
            RAISE EXCEPTION
                'ban appeal appellant lacks effective canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'ban_appeals_appellant_room_scope_chk';
        END IF;

        SELECT stream.owner_id, stream.room_id
          INTO locked_owner, locked_room
          FROM streams AS stream
         WHERE stream.id = NEW.stream_id
           FOR UPDATE;
        IF NOT FOUND
           OR locked_owner IS DISTINCT FROM canonical_owner
           OR locked_room IS DISTINCT FROM canonical_room THEN
            RAISE EXCEPTION 'ban appeal stream scope changed during authorization'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_stream_scope_chk';
        END IF;

        SELECT ban.ban_revision
          INTO canonical_revision
          FROM stream_bans AS ban
         WHERE ban.stream_id = NEW.stream_id
           AND ban.participant_id = NEW.appellant_id
           AND (ban.until IS NULL OR ban.until > clock_timestamp())
           FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'ban appeal requires an active ban'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_active_ban_chk';
        END IF;

        IF NEW.ban_revision IS NOT NULL
           AND NEW.ban_revision <> canonical_revision THEN
            RAISE EXCEPTION 'ban appeal revision does not match active ban'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_revision_scope_chk';
        END IF;
        NEW.ban_revision := canonical_revision;
        RETURN NEW;
    END IF;

    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.stream_id IS DISTINCT FROM OLD.stream_id
       OR NEW.appellant_id IS DISTINCT FROM OLD.appellant_id
       OR NEW.appeal_reason IS DISTINCT FROM OLD.appeal_reason
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.ban_revision IS DISTINCT FROM OLD.ban_revision THEN
        RAISE EXCEPTION 'ban appeal identity and submission are immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_identity_immutable_chk';
    END IF;
    IF OLD.status <> 'pending'
       OR NEW.status NOT IN ('approved', 'denied')
       OR NEW.reviewed_by IS NULL THEN
        RAISE EXCEPTION 'ban appeal decision transition is invalid'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_state_transition_chk';
    END IF;

    NEW.reviewed_at := clock_timestamp();
    NEW.decision_reason := NULLIF(btrim(NEW.decision_reason), '');
    IF NEW.decision_reason IS NOT NULL
       AND char_length(NEW.decision_reason) > 2000 THEN
        RAISE EXCEPTION 'ban appeal decision reason is too long'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_decision_reason_length_chk';
    END IF;

    request_actor := aero_live_governance_actor();
    IF request_actor IS NULL OR request_actor <> NEW.reviewed_by THEN
        RAISE EXCEPTION 'ban appeal reviewer does not match audit actor'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_reviewer_context_chk';
    END IF;

    -- Supported writers already own this row. NOWAIT prevents a raw UPDATE
    -- from inverting stream -> appeal order and deadlocking with them.
    SELECT stream.owner_id, stream.room_id
      INTO canonical_owner, canonical_room
      FROM streams AS stream
     WHERE stream.id = NEW.stream_id
       FOR UPDATE NOWAIT;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'ban appeal requires a canonical stream'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_stream_scope_chk';
    END IF;

    IF NEW.reviewed_by <> canonical_owner
       AND NOT EXISTS (
           SELECT 1
             FROM stream_moderators AS moderator
            WHERE moderator.stream_id = NEW.stream_id
              AND moderator.participant_id = NEW.reviewed_by
       ) THEN
        RAISE EXCEPTION 'ban appeal reviewer lacks current authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_reviewer_authority_chk';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = NEW.reviewed_by
           AND participant.deleted_at IS NULL
    ) THEN
        RAISE EXCEPTION 'ban appeal reviewer is inactive'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'ban_appeals_reviewer_identity_chk';
    END IF;

    IF canonical_room IS NOT NULL THEN
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE room.id = canonical_room;
        IF NOT FOUND
           OR NOT aero_participant_has_effective_workspace_access(
               canonical_workspace,
               NEW.reviewed_by
           )
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members AS member
                WHERE member.room_id = canonical_room
                  AND member.participant_id = NEW.reviewed_by
           ) THEN
            RAISE EXCEPTION 'ban appeal reviewer lacks canonical-room access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'ban_appeals_reviewer_room_scope_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS ban_appeal_transaction_fence
    ON ban_appeals;
CREATE TRIGGER ban_appeal_transaction_fence
    BEFORE INSERT OR UPDATE
    ON ban_appeals
    FOR EACH ROW
    EXECUTE FUNCTION ban_appeal_transaction_fence();

CREATE OR REPLACE FUNCTION ban_appeal_delete_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Participant erasure tombstones first, then removes the appellant's own
    -- free-text appeals. Outside that erasure path appeals are audit history.
    IF NOT EXISTS (
        SELECT 1
          FROM participants AS participant
         WHERE participant.id = OLD.appellant_id
           AND participant.deleted_at IS NULL
    ) THEN
        RETURN OLD;
    END IF;

    RAISE EXCEPTION 'ban appeal history is immutable while appellant is active'
        USING ERRCODE = '23514',
              CONSTRAINT = 'ban_appeals_delete_erasure_only_chk';
END
$$;

DROP TRIGGER IF EXISTS ban_appeal_delete_fence
    ON ban_appeals;
CREATE TRIGGER ban_appeal_delete_fence
    BEFORE DELETE
    ON ban_appeals
    FOR EACH ROW
    EXECUTE FUNCTION ban_appeal_delete_fence();

COMMENT ON COLUMN stream_bans.ban_revision IS
    'Server-owned monotonic incarnation id; changes on every insert or re-ban update';
COMMENT ON COLUMN ban_appeals.ban_revision IS
    'Ban incarnation observed and locked when this appeal was submitted; nullable only for legacy history';
COMMENT ON TRIGGER ban_appeal_transaction_fence ON ban_appeals IS
    'Binds new appeals to a locked active ban and enforces immutable scope plus authorized one-way review transitions';
COMMENT ON TRIGGER ban_appeal_delete_fence ON ban_appeals IS
    'Preserves appeal audit history except when participant erasure has already tombstoned or removed its appellant';
COMMENT ON TRIGGER live_governance_raid_compatibility ON raid_history IS
    'Database-owned timestamp prevents caller-controlled raid cooldown bypass';
COMMENT ON TRIGGER stream_moderator_delete_fence ON stream_moderators IS
    'Requires current canonical owner context for moderator revocation while permitting participant FK cascade';
COMMENT ON TRIGGER stream_ban_delete_fence ON stream_bans IS
    'Requires current owner/mod context for active unban while permitting expiry sweep and participant FK cascade';
COMMENT ON TRIGGER raid_history_delete_fence ON raid_history IS
    'Raid audit history is append-only';
