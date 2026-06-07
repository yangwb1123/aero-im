-- 0037 Keyword / highlight alerts (per-user keyword subscriptions).
--
-- A user subscribes to a keyword within a workspace; when a message whose text
-- contains that keyword is sent, the subscriber is notified. This table owns the
-- subscription CRUD only — the dispatch hook (notifying matching subscribers) is
-- wired separately into `ImService::dispatch_notifications`, which calls
-- `KeywordAlertRepo::matching_subscribers`.
--
-- Each subscription is owner-scoped (`participant_id`) and workspace-scoped. The
-- UNIQUE(participant_id, workspace_id, keyword) constraint makes a re-subscribe a
-- no-op (`ON CONFLICT DO NOTHING`); keywords are normalized (trimmed + lowercased)
-- before insert so matching is case-insensitive.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS keyword_alerts (
  id uuid PRIMARY KEY,
  participant_id uuid NOT NULL,
  workspace_id   uuid NOT NULL,
  keyword text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (participant_id, workspace_id, keyword)
);
CREATE INDEX IF NOT EXISTS keyword_alerts_ws_idx ON keyword_alerts (workspace_id);
