-- Migration 0157: Add version column to messages for optimistic locking.
-- Every insert starts at version=1; each edit atomically increments version.
-- The WHERE clause in the edit UPDATE includes version=N so two concurrent
-- edits cannot silently overwrite each other — the second gets 0 rows affected
-- and the server returns HTTP 409 Conflict.

ALTER TABLE messages
  ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
