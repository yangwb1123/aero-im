-- GDPR soft-delete: record when a participant's account was anonymised/deleted.
-- The row is kept to preserve FK integrity for audit records, workspace members,
-- and message history; the participant's message content is anonymised separately
-- (in the same application transaction). The index covers the common pattern of
-- filtering active participants in list/search queries.

ALTER TABLE participants ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ NULL;

CREATE INDEX IF NOT EXISTS participants_not_deleted
    ON participants (id)
    WHERE deleted_at IS NULL;
