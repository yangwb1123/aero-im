-- 0059_info_barriers.sql — information barriers / ethical walls (Purview-style).
--
-- An admin defines barred PAIRS of user-groups (migration 0033). Members across a
-- barred pair may not DM each other or share a channel: the barrier is evaluated
-- at the DM / group-DM creation points by joining this table against each
-- participant's `user_group_members` rows in BOTH orderings. The pair is
-- symmetric — `(group_a, group_b)` bars traffic in either direction.
--
-- `group_a` / `group_b` are plain columns (not cascading FKs): deleting a group
-- should not silently drop a compliance barrier — an admin removes the barrier
-- explicitly. Purely additive: a NEW `info_barriers` table; no existing table is
-- reshaped, and the whole script is idempotent (safe to re-run).
CREATE TABLE IF NOT EXISTS info_barriers (
    id           uuid        PRIMARY KEY,
    workspace_id uuid        NOT NULL,
    group_a      uuid        NOT NULL,
    group_b      uuid        NOT NULL,
    created_by   uuid        NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Per-workspace barrier listing + the `barred()` evaluation both filter by tenant.
CREATE INDEX IF NOT EXISTS info_barriers_ws_idx ON info_barriers (workspace_id);
