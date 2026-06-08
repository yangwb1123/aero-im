-- 0054 Legal hold / retention exemption (eDiscovery preservation).
--
-- An admin places a room (or, with a NULL `room_id`, a whole workspace) under a
-- legal hold. While an active hold covers a message's room — directly, or via the
-- room's workspace — the periodic retention sweep
-- (`WorkspaceRepo::sweep_expired_messages`, migration 0009) must NOT soft-delete
-- that message, so the data is preserved for eDiscovery. Complements the existing
-- per-workspace retention policy rather than replacing it.
--
-- Releasing a hold flips `active` to false and stamps `released_at`; the partial
-- index keeps lookups of the (few) currently-active holds cheap. Idempotent:
-- re-running is a no-op.
CREATE TABLE IF NOT EXISTS legal_holds (
  id uuid PRIMARY KEY,
  workspace_id uuid NOT NULL,
  room_id uuid,
  reason text NOT NULL,
  created_by uuid NOT NULL,
  active boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  released_at timestamptz
);
CREATE INDEX IF NOT EXISTS legal_holds_ws_idx ON legal_holds (workspace_id) WHERE active;
