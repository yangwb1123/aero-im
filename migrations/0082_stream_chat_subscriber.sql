-- 0082_stream_chat_subscriber.sql — subscriber badge on stream chat lines.
--
-- A stream-chat (danmaku) line gets an `is_subscriber` flag, set at insert time by
-- checking whether the sender has an active creator subscription to the stream's
-- owner (see `aero_storage::SubscriptionRepo::is_subscribed`, 0051). The flag is
-- surfaced on the returned/broadcast `StreamChatLine` so clients can render a
-- subscriber badge next to subscribers' messages (Twitch-style).
--
-- Purely additive + idempotent: a single nullable-default column on the existing
-- `stream_chat` table. Existing rows default to `false`; the column is filled
-- going forward on the chat-post path. No index needed (the flag is read back with
-- the line, never filtered on).
ALTER TABLE stream_chat
    ADD COLUMN IF NOT EXISTS is_subscriber BOOLEAN NOT NULL DEFAULT false;
