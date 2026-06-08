-- 0049 Channel bookmarks / header links (per-channel pinned resources).
--
-- Pinned links/resources shown in a channel's header (like Slack channel
-- bookmarks). A member adds a titled URL (with an optional emoji) to a room's
-- header bar; members list them in display order, and they can be edited or
-- removed. DISTINCT from message pins (`pins`) and personal saved items
-- (`bookmarks`): these belong to the channel, not to a message or a user.
--
-- Room-scoped: every row keys on `room_id`, and the server gates reads/mutates
-- through the standard room-access guard. Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS channel_bookmarks (
  id uuid PRIMARY KEY,
  room_id uuid NOT NULL,
  title text NOT NULL,
  url text NOT NULL,
  emoji text,
  created_by uuid NOT NULL,
  position int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS channel_bookmarks_room_position_idx
  ON channel_bookmarks (room_id, position);
