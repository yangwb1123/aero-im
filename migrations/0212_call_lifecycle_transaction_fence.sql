-- Linearizable call lifecycle backstops.
--
-- Production call writes now prove effective room access and mutate the
-- call/session leg under one transaction. These triggers protect raw SQL and
-- future writers from manufacturing a cross-room call participant, moving an
-- existing call aggregate, or reactivating a leg after the call ended.

CREATE OR REPLACE FUNCTION call_session_containment_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.room_id IS DISTINCT FROM OLD.room_id
           OR NEW.initiator IS DISTINCT FROM OLD.initiator
           OR NEW.kind IS DISTINCT FROM OLD.kind
           OR NEW.mode IS DISTINCT FROM OLD.mode
           OR NEW.started_at IS DISTINCT FROM OLD.started_at THEN
            RAISE EXCEPTION
                'call session aggregate identity is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'call_session_identity_immutable';
        END IF;
        RETURN NEW;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM room_members membership
         WHERE membership.room_id = NEW.room_id
           AND membership.participant_id = NEW.initiator
    ) THEN
        RAISE EXCEPTION
            'call initiator must belong to the canonical room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_session_initiator_room_containment';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS call_session_insert_containment
    ON call_sessions;
CREATE TRIGGER call_session_insert_containment
    BEFORE INSERT
    ON call_sessions
    FOR EACH ROW
    EXECUTE FUNCTION call_session_containment_guard();

DROP TRIGGER IF EXISTS call_session_identity_immutable
    ON call_sessions;
CREATE TRIGGER call_session_identity_immutable
    BEFORE UPDATE OF id, room_id, initiator, kind, mode, started_at
    ON call_sessions
    FOR EACH ROW
    EXECUTE FUNCTION call_session_containment_guard();

CREATE OR REPLACE FUNCTION call_participant_containment_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_room uuid;
    canonical_initiator uuid;
    canonical_ended_at timestamptz;
    requires_admission_check boolean;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.call_id IS DISTINCT FROM OLD.call_id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.role IS DISTINCT FROM OLD.role
       ) THEN
        RAISE EXCEPTION
            'call participant aggregate identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_participant_identity_immutable';
    END IF;

    IF TG_OP = 'INSERT' THEN
        requires_admission_check := true;
    ELSE
        requires_admission_check :=
            OLD.left_at IS NOT NULL AND NEW.left_at IS NULL;
    END IF;
    IF NOT requires_admission_check THEN
        RETURN NEW;
    END IF;

    SELECT session.room_id, session.initiator, session.ended_at
      INTO canonical_room, canonical_initiator, canonical_ended_at
      FROM call_sessions session
     WHERE session.id = NEW.call_id;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    IF canonical_ended_at IS NOT NULL AND NEW.left_at IS NULL THEN
        RAISE EXCEPTION
            'cannot admit a participant to an ended call'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_participant_active_call_required';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM room_members membership
         WHERE membership.room_id = canonical_room
           AND membership.participant_id = NEW.participant_id
    ) THEN
        RAISE EXCEPTION
            'call participant must belong to the canonical room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_participant_room_containment';
    END IF;

    IF (NEW.participant_id = canonical_initiator) <> (NEW.role = 'caller') THEN
        RAISE EXCEPTION
            'only the canonical initiator may own the caller leg'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_participant_caller_identity';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS call_participant_insert_containment
    ON call_participants;
CREATE TRIGGER call_participant_insert_containment
    BEFORE INSERT
    ON call_participants
    FOR EACH ROW
    EXECUTE FUNCTION call_participant_containment_guard();

DROP TRIGGER IF EXISTS call_participant_identity_immutable
    ON call_participants;
CREATE TRIGGER call_participant_identity_immutable
    BEFORE UPDATE OF call_id, participant_id, role, left_at
    ON call_participants
    FOR EACH ROW
    EXECUTE FUNCTION call_participant_containment_guard();

-- A room leave is an authorization revocation. Persistently close any live call
-- leg in the same transaction so later roster checks cannot observe a stale
-- active participant even if process-local/Redis cleanup is delayed.
CREATE OR REPLACE FUNCTION call_participant_close_on_room_leave()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE call_participants leg
       SET left_at = COALESCE(leg.left_at, NOW())
      FROM call_sessions session
     WHERE session.id = leg.call_id
       AND session.room_id = OLD.room_id
       AND leg.participant_id = OLD.participant_id
       AND leg.left_at IS NULL;
    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS call_participant_close_on_room_leave
    ON room_members;
CREATE TRIGGER call_participant_close_on_room_leave
    AFTER DELETE
    ON room_members
    FOR EACH ROW
    EXECUTE FUNCTION call_participant_close_on_room_leave();

CREATE INDEX IF NOT EXISTS call_participants_active_call_idx
    ON call_participants(call_id, participant_id)
    WHERE left_at IS NULL;

-- Caption persistence is itself a call mutation. Bind raw INSERTs to the
-- canonical live call, the speaker's current effective room access, and an
-- active durable leg. Historical lines intentionally remain after later
-- revocation/end; persisted line content and identity are immutable.
CREATE OR REPLACE FUNCTION call_transcript_containment_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_room uuid;
    canonical_ended_at timestamptz;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION
            'call transcript lines are immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_transcript_immutable';
    END IF;

    SELECT session.room_id, session.ended_at
      INTO canonical_room, canonical_ended_at
      FROM call_sessions session
     WHERE session.id = NEW.call_id;
    IF NOT FOUND OR canonical_ended_at IS NOT NULL THEN
        RAISE EXCEPTION
            'call transcript requires a live canonical call'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_transcript_live_call_required';
    END IF;

    IF NOT aero_effective_room_access(canonical_room, NEW.speaker_id, NULL) THEN
        RAISE EXCEPTION
            'call transcript speaker lacks effective room access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_transcript_room_access';
    END IF;

    PERFORM 1
      FROM call_sessions session
     WHERE session.id = NEW.call_id
       AND session.room_id = canonical_room
       AND session.ended_at IS NULL
       FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'call transcript requires a live canonical call'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_transcript_live_call_required';
    END IF;

    PERFORM 1
      FROM call_participants leg
     WHERE leg.call_id = NEW.call_id
       AND leg.participant_id = NEW.speaker_id
       AND leg.left_at IS NULL
       FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'call transcript speaker must have an active call leg'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_transcript_active_leg_required';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS call_transcript_insert_containment
    ON call_transcripts;
CREATE TRIGGER call_transcript_insert_containment
    BEFORE INSERT
    ON call_transcripts
    FOR EACH ROW
    EXECUTE FUNCTION call_transcript_containment_guard();

DROP TRIGGER IF EXISTS call_transcript_identity_immutable
    ON call_transcripts;
CREATE TRIGGER call_transcript_identity_immutable
    BEFORE UPDATE
    ON call_transcripts
    FOR EACH ROW
    EXECUTE FUNCTION call_transcript_containment_guard();

COMMENT ON FUNCTION call_session_containment_guard() IS
    'Raw-SQL backstop for immutable call identity and initiator/room containment';
COMMENT ON FUNCTION call_participant_containment_guard() IS
    'Raw-SQL backstop for active-call admission and canonical room/caller containment';
COMMENT ON FUNCTION call_transcript_containment_guard() IS
    'Raw-SQL backstop for live-call caption authorization and immutable transcript lines';
