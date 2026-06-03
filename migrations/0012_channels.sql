-- Aero IM — channel management (public/private, archive, topic/description).
-- Additive + idempotent: extends the existing `rooms` table (0001_init.sql,
-- workspace_id added by 0006_workspaces.sql) with channel metadata so a room of
-- kind 'channel' can be public (discoverable + joinable by any workspace member),
-- archived (hidden from browsing), and carry a topic + description.
--
-- Channels default to PRIVATE so the new column never silently widens visibility
-- of any pre-existing room. Browsing is per-workspace, so the partial index keys
-- on workspace_id over only the rows the discovery query returns.

ALTER TABLE rooms
    ADD COLUMN IF NOT EXISTS is_private   BOOLEAN NOT NULL DEFAULT true,
    ADD COLUMN IF NOT EXISTS is_archived  BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS topic        TEXT,
    ADD COLUMN IF NOT EXISTS description  TEXT;

-- Hot path for "browse joinable channels in this workspace": only public,
-- non-archived rooms are listed, so the index is partial over exactly that set.
CREATE INDEX IF NOT EXISTS rooms_public_channels_idx
    ON rooms (workspace_id)
    WHERE is_private = false AND is_archived = false;
