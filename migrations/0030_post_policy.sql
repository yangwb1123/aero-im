-- Aero IM — announcement channels (per-room post policy).
-- Additive + idempotent: extends the existing `rooms` table (0001_init.sql,
-- channel metadata added by 0012_channels.sql) with a `post_policy` so a room can
-- be restricted to "announcements only".
--
-- Valid values:
--   'everyone' (default) — any room member may post (unchanged behavior).
--   'admins'             — only the room creator OR a workspace Admin/Owner may
--                          post; everyone else can read but not post.
--
-- Defaulting to 'everyone' keeps the hot send path correct for every pre-existing
-- room — the column never silently tightens who can post in any current channel.

ALTER TABLE rooms
    ADD COLUMN IF NOT EXISTS post_policy TEXT NOT NULL DEFAULT 'everyone';
