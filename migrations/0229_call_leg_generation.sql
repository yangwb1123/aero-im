-- Fence delayed per-leg cleanup against a participant reconnecting to the
-- same canonical call. Existing legs are the first durable incarnation.
ALTER TABLE call_participants
    ADD COLUMN IF NOT EXISTS leg_generation BIGINT NOT NULL DEFAULT 1;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'call_participants'::regclass
           AND conname = 'call_participants_leg_generation_positive'
    ) THEN
        ALTER TABLE call_participants
            ADD CONSTRAINT call_participants_leg_generation_positive
            CHECK (leg_generation > 0);
    END IF;
END
$$;
