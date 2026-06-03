-- 0013_webhooks.sql — incoming + outgoing webhooks (integration plane)
--
-- Two tables, one per direction:
--   * incoming_webhooks: an inbound URL credential (a hashed token) that lets an
--     external system POST a message into a room, posted as a dedicated bot
--     participant. We store only the SHA-256 hash of the token; the plaintext is
--     shown once at creation and never persisted.
--   * outgoing_webhooks: an external URL we POST room events to (HMAC-signed with
--     a per-hook secret), optionally filtered to a set of event kinds.
--
-- Purely additive — no existing table is touched. Both cascade-delete with their
-- room. `revoked_at` is a soft tombstone so a leaked token/secret can be disabled
-- without losing its row.

CREATE TABLE IF NOT EXISTS incoming_webhooks (
    id          UUID        PRIMARY KEY,
    room_id     UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    bot_id      UUID        NOT NULL REFERENCES participants(id),
    -- SHA-256 hex of the bearer token. Unique so a lookup is a single index probe
    -- and a token authenticates exactly one hook.
    token_hash  TEXT        NOT NULL UNIQUE,
    label       TEXT,
    created_by  UUID        REFERENCES participants(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);

-- The inbound POST path looks a hook up by the hash of its presented token; this
-- index serves that single hot lookup. (UNIQUE above already creates an index,
-- but name it explicitly for idempotency parity with the rest of the schema.)
CREATE INDEX IF NOT EXISTS incoming_webhooks_token_hash_idx
    ON incoming_webhooks (token_hash);

CREATE TABLE IF NOT EXISTS outgoing_webhooks (
    id          UUID        PRIMARY KEY,
    room_id     UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    url         TEXT        NOT NULL,
    -- Per-hook HMAC secret used to sign each delivery (X-Aero-Signature).
    secret      TEXT        NOT NULL,
    -- Event-kind filter. Empty ⇒ deliver every event kind.
    events      TEXT[]      NOT NULL DEFAULT '{}',
    label       TEXT,
    created_by  UUID        REFERENCES participants(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);

-- Dispatch fans out per room: "active outgoing hooks for this room", so index on
-- room_id.
CREATE INDEX IF NOT EXISTS outgoing_webhooks_room_idx
    ON outgoing_webhooks (room_id);
