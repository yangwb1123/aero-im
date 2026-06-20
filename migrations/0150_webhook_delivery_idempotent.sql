-- 0150_webhook_delivery_idempotent.sql
-- Idempotency guard for OUTGOING webhook delivery.
--
-- A (webhook_id, event_id) pair uniquely identifies one event→endpoint delivery.
-- JetStream is at-least-once, so a consumer that crashes (or loses its ack) after
-- POSTing but before acking re-dispatches the SAME RoomEvent — and the dispatcher
-- would POST it to the receiver's URL a SECOND time (duplicate external delivery).
--
-- This partial unique index lets `record_attempt` claim the (webhook_id, event_id)
-- atomically via `INSERT ... ON CONFLICT DO NOTHING RETURNING id`: the first
-- dispatch inserts + sends; a redelivery hits the conflict, gets no row back, and
-- skips the duplicate POST. `event_id` is NULL for events that carry no correlation
-- id (currently only RoomEvent::Message supplies one — the message id); those rows
-- are EXCLUDED from the index, so their behavior is unchanged (no dedup).
CREATE UNIQUE INDEX IF NOT EXISTS webhook_delivery_event_idem_idx
    ON webhook_delivery_log (webhook_id, event_id)
    WHERE event_id IS NOT NULL;
