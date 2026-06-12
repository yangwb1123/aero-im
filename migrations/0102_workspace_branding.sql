-- Migration 0102: workspace branding fields
-- Adds logo URL, color scheme, custom domain, and description to workspaces.

ALTER TABLE workspaces ADD COLUMN logo_url TEXT;
ALTER TABLE workspaces ADD COLUMN color_scheme TEXT;
ALTER TABLE workspaces ADD COLUMN custom_domain TEXT;
ALTER TABLE workspaces ADD COLUMN description TEXT;
