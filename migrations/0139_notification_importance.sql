-- Notification importance score (方向五 — 智能通知优先级).
-- Existing rows default to 0.5 (SavedSearch-level), preserving backward
-- compatibility until the background backfill or next notify writes a score.

ALTER TABLE notifications
  ADD COLUMN importance_score REAL NOT NULL DEFAULT 0.5;
