-- Aero IM — P1 initial schema.
-- Aligned with crates/aero-common/src/model.rs and ids.rs (UUID = u128 from Ulid).
-- See docs/specs/2026-05-22-aero-im-design.md §3.3.

-- Extensions ----------------------------------------------------------------

CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS vector;

-- Participants --------------------------------------------------------------

CREATE TABLE IF NOT EXISTS participants (
    id           UUID        PRIMARY KEY,
    kind         TEXT        NOT NULL CHECK (kind IN ('human', 'agent', 'bot')),
    display_name TEXT        NOT NULL,
    avatar_url   TEXT,
    created_by   UUID        REFERENCES participants(id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS participants_created_by_idx
    ON participants(created_by)
    WHERE created_by IS NOT NULL;

-- Credentials (Human-only authentication material) -------------------------

CREATE TABLE IF NOT EXISTS credentials (
    participant_id UUID        PRIMARY KEY
                                REFERENCES participants(id) ON DELETE CASCADE,
    email          CITEXT      NOT NULL UNIQUE,
    password_hash  TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login     TIMESTAMPTZ
);

-- Rooms ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS rooms (
    id         UUID        PRIMARY KEY,
    kind       TEXT        NOT NULL CHECK (kind IN ('direct', 'group', 'channel')),
    name       TEXT,
    created_by UUID        NOT NULL REFERENCES participants(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS rooms_created_by_idx ON rooms(created_by);

-- Room membership (simple ReBAC) -------------------------------------------

CREATE TABLE IF NOT EXISTS room_members (
    room_id        UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    role           TEXT        NOT NULL CHECK (role IN ('owner', 'member', 'admin')),
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (room_id, participant_id)
);

CREATE INDEX IF NOT EXISTS room_members_participant_idx
    ON room_members(participant_id);

-- Messages ------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS messages (
    id         UUID        PRIMARY KEY,
    room_id    UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    sender_id  UUID        NOT NULL REFERENCES participants(id),
    blocks     JSONB       NOT NULL,
    reply_to   UUID        REFERENCES messages(id),
    metadata   JSONB       NOT NULL DEFAULT '{}'::jsonb,
    embedding  vector(1024),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    edited_at  TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ
);

-- Hot read path: latest messages per room.
CREATE INDEX IF NOT EXISTS messages_room_created_idx
    ON messages(room_id, created_at DESC);

-- Block-shape inspection (mentions, file_id lookups, etc.).
CREATE INDEX IF NOT EXISTS messages_blocks_gin
    ON messages USING gin (blocks);

-- Semantic search (P2 will populate the embedding column).
-- Created up-front so the index already exists once embeddings start flowing.
CREATE INDEX IF NOT EXISTS messages_embedding_hnsw
    ON messages USING hnsw (embedding vector_cosine_ops);

CREATE INDEX IF NOT EXISTS messages_reply_to_idx
    ON messages(reply_to)
    WHERE reply_to IS NOT NULL;
