-- Migration 0118: per-room cap on the number of distinct emoji a single user
-- may add to any one message.  NULL means no limit.
ALTER TABLE rooms ADD COLUMN max_reactions_per_user INT;
