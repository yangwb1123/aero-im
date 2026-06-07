-- 0043 Channel join requests (request-to-join with owner/admin approval).
--
-- A workspace member asks to join a channel they are not yet in; the channel's
-- creator (owner) or a workspace admin then approves or denies the request.
-- Each row is one request: it starts `pending` and transitions once to
-- `approved` or `denied`, capturing who decided and when. A partial unique index
-- keeps at most one OUTSTANDING (`pending`) request per (room, requester) so a
-- repeated ask is idempotent, while still allowing a fresh request after a prior
-- one was decided. Pure additive metadata over existing rooms: membership is
-- still granted through the existing `room_members` path on approval.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS channel_join_requests (
  id uuid PRIMARY KEY,
  room_id uuid NOT NULL,
  requester_id uuid NOT NULL,
  status text NOT NULL DEFAULT 'pending',
  created_at timestamptz NOT NULL DEFAULT now(),
  decided_at timestamptz,
  decided_by uuid
);
CREATE UNIQUE INDEX IF NOT EXISTS channel_join_requests_pending_uq
  ON channel_join_requests (room_id, requester_id)
  WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS channel_join_requests_room_idx
  ON channel_join_requests (room_id, status);
