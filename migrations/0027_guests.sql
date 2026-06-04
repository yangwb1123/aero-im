-- Aero IM — Single-channel guest accounts (To-B external collaborators).
--
-- A "guest" is a workspace member flagged `is_guest = true`. Their workspace
-- ROLE stays `member` (the role column already CHECK-constrains to
-- owner/admin/member/guest), but the boolean flag is the lightweight, explicit
-- marker the join path enforces against: a guest is added ONLY to the specific
-- channel(s) they were invited to and may NOT self-join other (public) channels.
--
-- This migration is ADDITIVE and IDEMPOTENT: the column add is guarded with
-- `IF NOT EXISTS` and the index with `IF NOT EXISTS`, so re-running it is a
-- no-op. No backfill is needed — existing members default to non-guest.

-- The guest flag on workspace membership. Defaults false so every pre-existing
-- (and every future ordinary) member is a full member, not a guest.
ALTER TABLE workspace_members
    ADD COLUMN IF NOT EXISTS is_guest BOOLEAN NOT NULL DEFAULT false;

-- Partial index for the "list / detect guests in a workspace" lookups. Partial
-- (WHERE is_guest) keeps it tiny — only the handful of guest rows are indexed,
-- not the full membership table — while still serving `list_guests` /
-- `is_guest` cheaply.
CREATE INDEX IF NOT EXISTS workspace_members_guest_idx
    ON workspace_members(workspace_id)
    WHERE is_guest;
