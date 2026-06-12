-- Migration 0104: custom raid message
-- Adds an optional message column to raid_history for a custom shoutout message.

ALTER TABLE raid_history ADD COLUMN message TEXT;
