-- Idempotency token for NotifyBatch delivery (ROADMAP TODO: documented limitation).
--
-- RoomEvent::NotifyBatch carries no idempotency token, so if the WS consumer
-- crashes before ack, NATS redelivers the event and the recipients are re-expanded
-- into duplicate `notifications` rows (only `id` is unique). A naive unique key
-- on (participant_id, message_id, kind) is WRONG — two people reacting to your
-- message legitimately produce two Reaction notifications for the same recipient.
--
-- Fix: each NotifyBatch publish carries a monotonically injected `delivery_id`
-- (ULID). On the consuming side, `notifications.insert_many` uses
-- `ON CONFLICT (delivery_id, participant_id) DO NOTHING` — a redelivered batch
-- with the same delivery_id produces zero new rows for participants already
-- notified in the original delivery. The partial unique index covers only rows
-- with a non-null delivery_id, so existing notifications (delivery_id IS NULL)
-- are unaffected and old batches continue to work.

-- 1. Add the delivery_id column (nullable, default NULL for backward compatibility).
ALTER TABLE notifications
    ADD COLUMN IF NOT EXISTS delivery_id UUID;

-- 2. Partial unique index: (delivery_id, participant_id) where delivery_id IS NOT
--    NULL. A NULL delivery_id means "no batch" (existing rows), never conflicts.
CREATE UNIQUE INDEX IF NOT EXISTS uq_notifications_delivery_participant
    ON notifications (delivery_id, participant_id)
    WHERE delivery_id IS NOT NULL;
