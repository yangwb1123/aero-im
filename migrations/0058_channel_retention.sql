-- 0058_channel_retention.sql — per-channel message-retention override.
--
-- Retention was workspace-only (0009 added `workspaces.retention_days`, and the
-- periodic sweep `WorkspaceRepo::sweep_expired_messages` keys off it). This adds
-- an OPTIONAL per-room override so e.g. #legal can keep messages forever while
-- #random purges after 30 days. A room's own `retention_days` takes precedence
-- over the workspace default; NULL means "inherit the workspace default", so the
-- effective window is `COALESCE(rooms.retention_days, workspaces.retention_days)`.
--
-- NULL on every existing/new room ⇒ unchanged behavior (inherit the tenant
-- policy) until someone sets an override. Purely additive and idempotent
-- (`IF NOT EXISTS`, safe to re-run).

ALTER TABLE rooms ADD COLUMN IF NOT EXISTS retention_days int;  -- NULL = inherit workspace default
