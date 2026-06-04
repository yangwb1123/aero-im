-- 0029 Scheduled streams (live-event announcements).
--
-- A workspace member announces an upcoming live stream ahead of time (title,
-- optional description, optional associated room, scheduled-for time). Members
-- list upcoming announcements; the creator can cancel one. This is purely the
-- announcement/lifecycle record — actually going live still uses the existing
-- `/api/streams` ingest path; nothing here touches media transport.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS scheduled_streams (
  id            uuid PRIMARY KEY,
  workspace_id  uuid NOT NULL,
  room_id       uuid,
  title         text NOT NULL,
  description   text,
  scheduled_for timestamptz NOT NULL,
  created_by    uuid NOT NULL,
  status        text NOT NULL DEFAULT 'scheduled',  -- scheduled | live | canceled | ended
  created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS scheduled_streams_ws_time_idx ON scheduled_streams (workspace_id, scheduled_for);
