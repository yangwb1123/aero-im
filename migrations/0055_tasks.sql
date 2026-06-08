-- 0055 Tasks / to-do tracker (Lark Tasks / Teams Tasks-lite).
--
-- Lightweight, durable task tracking attached to a room. A member creates a task
-- (optionally anchored to a source message), assigns it to someone, sets a due
-- date, and marks it done. Distinct from the AI action-item EXTRACTION endpoint
-- (which only summarizes): these rows are durable, assignable, and stateful.
--
-- `status` is a short text domain: 'open' | 'in_progress' | 'done' (validated at
-- the edge). `assignee_id`, `source_message_id`, and `due_at` are optional.
-- Access is gated entirely at the server layer via `assert_room_access` on
-- `room_id` — this table owns only the task CRUD. Idempotent: re-running is a
-- no-op.
CREATE TABLE IF NOT EXISTS tasks (
  id uuid PRIMARY KEY,
  room_id uuid NOT NULL,
  creator_id uuid NOT NULL,
  assignee_id uuid,
  title text NOT NULL,
  source_message_id uuid,
  status text NOT NULL DEFAULT 'open',
  due_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS tasks_room_idx ON tasks (room_id, status);
CREATE INDEX IF NOT EXISTS tasks_assignee_idx ON tasks (assignee_id) WHERE assignee_id IS NOT NULL;
