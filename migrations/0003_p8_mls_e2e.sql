-- Aero IM — P8 MLS (Messaging Layer Security) end-to-end encryption scaffold.
-- Adds the storage for OpenMLS-style KeyPackages and group state, plus an
-- encrypted-payload column on messages.
--
-- The crypto itself (handshakes, ratchets, AEAD) lives in an MLS library
-- (openmls). This migration only persists the wire artifacts and lets the
-- server route + relay them without inspecting the contents.

-- KeyPackages: published once per (participant, device); consumed by joiners.
CREATE TABLE IF NOT EXISTS mls_key_packages (
    id              UUID         PRIMARY KEY,
    participant_id  UUID         NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    ciphersuite     TEXT         NOT NULL,
    payload         BYTEA        NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    consumed_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS mls_kp_participant_idx
    ON mls_key_packages(participant_id)
    WHERE consumed_at IS NULL;

-- MLS groups: one row per cryptographic group (typically 1:1 with `rooms`).
CREATE TABLE IF NOT EXISTS mls_groups (
    group_id     BYTEA       PRIMARY KEY,
    room_id      UUID        REFERENCES rooms(id) ON DELETE CASCADE,
    ciphersuite  TEXT        NOT NULL,
    epoch        BIGINT      NOT NULL DEFAULT 0,
    state        BYTEA       NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS mls_groups_room_idx
    ON mls_groups(room_id);

-- Add encrypted payload + group reference to messages (nullable; opt-in per-room).
ALTER TABLE messages
    ADD COLUMN IF NOT EXISTS mls_group_id BYTEA,
    ADD COLUMN IF NOT EXISTS mls_epoch    BIGINT,
    ADD COLUMN IF NOT EXISTS mls_payload  BYTEA;

CREATE INDEX IF NOT EXISTS messages_mls_group_idx
    ON messages(mls_group_id)
    WHERE mls_group_id IS NOT NULL;
