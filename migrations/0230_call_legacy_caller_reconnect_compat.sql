-- Keep the v3 group-call reconnect upsert compatible while v4 nodes are
-- rolling out. v3 re-submits every join with role='member', including the
-- canonical initiator. PostgreSQL runs BEFORE INSERT triggers before resolving
-- ON CONFLICT, so the 0212 containment trigger rejected that harmless conflict
-- attempt even though the existing durable row already had role='caller'.
--
-- The exception below is deliberately narrow: it applies only to INSERT, only
-- when the same call/participant row already exists as the canonical caller,
-- and still runs the active-call and room-membership checks. A raw writer
-- cannot create a new initiator-as-member row; without the pre-existing caller
-- row it continues to fail closed.
CREATE OR REPLACE FUNCTION call_participant_containment_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_room uuid;
    canonical_initiator uuid;
    canonical_ended_at timestamptz;
    requires_admission_check boolean;
    legacy_caller_conflict boolean;
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

    legacy_caller_conflict := false;
    IF TG_OP = 'INSERT'
       AND NEW.participant_id = canonical_initiator
       AND NEW.role = 'member' THEN
        -- Keep the conflict target alive until this statement reaches its
        -- ON CONFLICT action. Without the row lock, a concurrent DELETE could
        -- turn the narrowly allowed conflict attempt into a real member INSERT.
        PERFORM 1
          FROM call_participants existing
         WHERE existing.call_id = NEW.call_id
           AND existing.participant_id = NEW.participant_id
           AND existing.role = 'caller'
         FOR KEY SHARE;
        legacy_caller_conflict := FOUND;
    END IF;

    IF (NEW.participant_id = canonical_initiator) <> (NEW.role = 'caller')
       AND NOT legacy_caller_conflict THEN
        RAISE EXCEPTION
            'only the canonical initiator may own the caller leg'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'call_participant_caller_identity';
    END IF;
    RETURN NEW;
END
$$;

COMMENT ON FUNCTION call_participant_containment_guard() IS
    'Raw-SQL backstop for active-call admission and canonical room/caller containment; permits only conflict-safe v3 initiator reconnect upserts';
