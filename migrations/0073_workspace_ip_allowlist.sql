-- 0073_workspace_ip_allowlist.sql — workspace IP / network allowlist (authorized
-- networks).
--
-- An admin declares the CIDR ranges allowed to reach a workspace's data (think
-- "office network only"). Each row is one authorized network for a tenant, with
-- an optional human note. An EMPTY allowlist for a workspace means "allow all"
-- (the feature is disabled until at least one CIDR is added), so existing
-- workspaces keep working with no rows.
--
-- CIDR matching is performed in the application layer (a hand-rolled v4/v6 prefix
-- match in `aero_storage::ip_allowlist::ip_in_cidr`) so the `cidr` column is plain
-- TEXT — no Postgres `inet`/`cidr` type dependency or operator-class coupling.
-- `UNIQUE(workspace_id, cidr)` makes adding the same range idempotent. Purely
-- additive: a NEW table; no existing table is reshaped, and the script is
-- idempotent (safe to re-run).
CREATE TABLE IF NOT EXISTS workspace_ip_allowlist (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    cidr         text        NOT NULL,
    note         text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, cidr)
);

-- Per-workspace listing + the enforcement lookup both filter by tenant.
CREATE INDEX IF NOT EXISTS workspace_ip_allowlist_ws_idx
    ON workspace_ip_allowlist (workspace_id);
