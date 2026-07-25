-- 0159_workspace_region.sql — Data residency: per-workspace storage region
-- (ROADMAP 第六版 · 方向五·1).
--
-- Adds a `region_code` column to workspaces so admins can pin a tenant's data
-- (blobs, exports) to a specific geographic region for compliance (GDPR,
-- data-sovereignty, FedRAMP). The region is a free-form short string matching
-- the keys in the `[storage_regions]` config block; absent/empty means "use the
-- default backend".
--
-- Purely additive: no existing code that reads `workspaces` will break; the
-- column defaults to NULL, which existing rows keep (they use the default store).

ALTER TABLE workspaces
    ADD COLUMN IF NOT EXISTS region_code VARCHAR(16);
