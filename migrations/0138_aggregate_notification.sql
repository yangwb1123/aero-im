-- Aggregate notification count column (ROADMAP7 方向三: 通知聚合).
--
-- A single AggregatedReply notification folds N individual Reply events into
-- one row with `aggregate_count = N`. The client renders it as
-- "N replies in thread" instead of N separate notification rows.

ALTER TABLE notifications
    ADD COLUMN IF NOT EXISTS aggregate_count INTEGER;
