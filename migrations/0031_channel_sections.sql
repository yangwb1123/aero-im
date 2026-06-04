-- Aero IM — per-user channel sidebar sections.
-- Slack/Teams "sections": a user organizes their channel sidebar into named,
-- ordered sections and assigns channels (rooms) to them. Sections are PRIVATE to
-- the owning participant and scoped to a workspace — pure organizational metadata
-- over existing rooms. Additive + idempotent: two NEW tables, no existing table
-- is touched.

CREATE TABLE IF NOT EXISTS channel_sections (
  id uuid PRIMARY KEY,
  participant_id uuid NOT NULL,
  workspace_id   uuid NOT NULL,
  name text NOT NULL,
  position int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS channel_sections_owner_idx ON channel_sections (participant_id, workspace_id, position);

CREATE TABLE IF NOT EXISTS channel_section_items (
  section_id uuid NOT NULL REFERENCES channel_sections(id) ON DELETE CASCADE,
  room_id    uuid NOT NULL,
  position int NOT NULL DEFAULT 0,
  PRIMARY KEY (section_id, room_id)
);
