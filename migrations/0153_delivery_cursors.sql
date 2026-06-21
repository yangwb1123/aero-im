-- 0153_delivery_cursors.sql — per-(participant, room) persistent DELIVERY cursor
-- (ROADMAP 第六版 · 方向三·A · 每用户持久投递台账).
--
-- Today reconnect backfill (`/ws?since=<message_id>`) applies a SINGLE global
-- cursor to EVERY room the participant belongs to, and the only per-(participant,
-- room) position that exists is `read_receipts.last_read_message_id` — the
-- *human-seen* cursor (advanced on visual "mark read"). A message can be
-- delivered to the client but not yet read; the seen cursor cannot express that,
-- so reconnect cannot resume each room from "what this client already HAS".
--
-- This table is the per-room delivery Last-Known-Good (LKG): the client ACKs what
-- it has durably received (`delivery_ack {room_id, message_id, seq}`), advancing a
-- monotonic per-room cursor. On reconnect the server backfills each room from its
-- own cursor (O(rooms) targeted, not one global id replayed everywhere), and a
-- second device sharing the same (participant, room) cursor only sees messages
-- newer than the first device's ACK — collapsing per-device re-replay.
--
-- Distinct from `read_receipts` (seen, for unread badges / "Seen by") and from
-- `message_receipts` (precise per-message reader set): this is the delivery
-- ledger that drives reconnect catch-up. Purely additive — no existing table is
-- touched. ON DELETE CASCADE on participant aligns with GDPR erasure (a deleted
-- participant's cursors vanish); on room aligns with room delete / archival.

CREATE TABLE IF NOT EXISTS delivery_cursors (
    participant_id            UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id                   UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    -- The message id of the highest-seq line the client has durably received;
    -- the backfill floor on reconnect (`list_since(room, this_id, cap)`).
    last_delivered_message_id UUID        NOT NULL,
    -- The per-subject bus seq the client dedupes on. The MONOTONIC guard: an ACK
    -- only advances the cursor when its seq strictly exceeds the stored one, so
    -- at-least-once redelivery and racing multi-device ACKs converge to max(seq).
    last_seq                  BIGINT      NOT NULL DEFAULT 0,
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, room_id)
);

-- Sweep / clear by room (legal-hold + retention edge case: when a room's history
-- is held or swept, its cursors are cleared so a preserved message is never
-- skipped past on the next reconnect).
CREATE INDEX IF NOT EXISTS delivery_cursors_room_idx ON delivery_cursors (room_id);
