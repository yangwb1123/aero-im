-- 0048 Channel canvas (per-channel collaborative document).
--
-- A channel ("room") can own multiple canvases — Slack Canvas / Lark
-- Docs-in-channel. Each canvas is a titled rich document whose body is a JSON
-- array of arbitrary blocks (stored as JSONB, NOT coupled to `aero_common::Block`).
-- It is collaborative: any member with room access may edit it.
--
-- `room_id` / `author_id` mirror the ULID-backed `RoomId` / `ParticipantId`
-- (persisted as uuid). `updated_at` is bumped on every edit so the room's
-- canvases can be listed newest-edit-first. Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS channel_canvases (
  id uuid PRIMARY KEY,
  room_id uuid NOT NULL,
  author_id uuid NOT NULL,
  title text NOT NULL,
  blocks jsonb NOT NULL DEFAULT '[]'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS channel_canvases_room_idx ON channel_canvases (room_id);
