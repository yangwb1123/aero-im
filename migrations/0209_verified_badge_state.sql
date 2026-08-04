-- Make the timestamp/badge pair canonical for both historical rows and every
-- future write. Actor authorization remains transaction-owned by
-- ParticipantRepo::set_verified_authorized because SQL has no request identity.
UPDATE participants
   SET verified_at = CASE
       WHEN is_verified THEN COALESCE(verified_at, NOW())
       ELSE NULL
   END
 WHERE (is_verified AND verified_at IS NULL)
    OR (NOT is_verified AND verified_at IS NOT NULL);

ALTER TABLE participants
    DROP CONSTRAINT IF EXISTS participants_verified_state_chk;
ALTER TABLE participants
    ADD CONSTRAINT participants_verified_state_chk
    CHECK (
        (is_verified AND verified_at IS NOT NULL)
        OR (NOT is_verified AND verified_at IS NULL)
    );
