-- 0052 Out-of-office / auto-responder (per-user).
--
-- A user sets an out-of-office status: a free-text message plus an optional
-- active window. While active, an out-of-band bus-listener bot
-- (`ooo_bot`) posts the message ONCE per sender into a 1:1 DM room when
-- someone messages the absent user — never on the message hot path.
--
-- `out_of_office` is keyed by `participant_id` (one OOO per user); `set` upserts.
-- `ooo_auto_replies` dedupes the bot's replies: one row per (OOO-user, sender)
-- pair so a given sender is auto-replied at most once per OOO period.
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS out_of_office (
  participant_id uuid PRIMARY KEY,
  message text NOT NULL,
  starts_at timestamptz,
  ends_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS ooo_auto_replies (
  ooo_participant_id uuid NOT NULL,
  sender_id uuid NOT NULL,
  room_id uuid NOT NULL,
  replied_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (ooo_participant_id, sender_id)
);

CREATE INDEX IF NOT EXISTS ooo_auto_replies_ooo_idx ON ooo_auto_replies (ooo_participant_id);
