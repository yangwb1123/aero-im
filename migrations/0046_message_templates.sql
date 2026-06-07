-- 0046 Message templates / canned responses (per-user).
--
-- A user saves a reusable message body (a JSON array of blocks) under a name,
-- then lists, deletes, or posts it into a room with one call. Posting replays
-- the stored blocks through the normal send path (`ImService::send_message`),
-- so the usual room-membership / post-policy / moderation gates still apply.
--
-- Owner-scoped: every read/mutate keys on `participant_id`, so a caller can
-- only ever see, delete, or send their own templates. Idempotent: re-running
-- is a no-op.
CREATE TABLE IF NOT EXISTS message_templates (
  id uuid PRIMARY KEY,
  participant_id uuid NOT NULL,
  name text NOT NULL,
  blocks jsonb NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS message_templates_owner_idx ON message_templates (participant_id);
