-- Aero IM — P2 collaboration + AI-Ready schema.
-- Adds: searchable_text + tsvector on messages, read_receipts, reactions, blobs,
--       ai_jobs, streams, call_sessions.
-- See docs/specs/2026-05-22-aero-im-design.md §6 "P2 — 多房间/私聊/历史/已读/附件 + RAG 语义搜索 + 摘要"

-- Extensions ----------------------------------------------------------------

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Messages: searchable_text + FTS column + soft-edit metadata --------------

ALTER TABLE messages
    ADD COLUMN IF NOT EXISTS searchable_text TEXT NOT NULL DEFAULT '';

ALTER TABLE messages
    ADD COLUMN IF NOT EXISTS search_tsv tsvector
        GENERATED ALWAYS AS (to_tsvector('simple', coalesce(searchable_text, ''))) STORED;

CREATE INDEX IF NOT EXISTS messages_search_tsv_gin
    ON messages USING gin (search_tsv);

CREATE INDEX IF NOT EXISTS messages_searchable_trgm
    ON messages USING gin (searchable_text gin_trgm_ops);

-- Read receipts -------------------------------------------------------------

CREATE TABLE IF NOT EXISTS read_receipts (
    room_id               UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    participant_id        UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    last_read_message_id  UUID        NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (room_id, participant_id)
);

CREATE INDEX IF NOT EXISTS read_receipts_room_updated_idx
    ON read_receipts(room_id, updated_at DESC);

-- Reactions -----------------------------------------------------------------

CREATE TABLE IF NOT EXISTS reactions (
    message_id     UUID        NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    emoji          TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (message_id, participant_id, emoji)
);

CREATE INDEX IF NOT EXISTS reactions_message_idx ON reactions(message_id);

-- Blobs (S3/MinIO attachments) ---------------------------------------------

CREATE TABLE IF NOT EXISTS blobs (
    id           UUID        PRIMARY KEY,
    owner_id     UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    kind         TEXT        NOT NULL CHECK (kind IN ('image','video','audio','document','other')),
    name         TEXT        NOT NULL,
    mime         TEXT        NOT NULL,
    size         BIGINT      NOT NULL,
    sha256       TEXT,
    storage_key  TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finalized_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS blobs_owner_idx ON blobs(owner_id);

-- AI jobs queue (embed/summarize/moderate/answer) --------------------------

CREATE TABLE IF NOT EXISTS ai_jobs (
    id           UUID        PRIMARY KEY,
    kind         TEXT        NOT NULL CHECK (kind IN ('embed','summarize','moderate','answer')),
    target_id    UUID,
    status       TEXT        NOT NULL DEFAULT 'queued'
                              CHECK (status IN ('queued','running','done','failed','dead')),
    attempts     INTEGER     NOT NULL DEFAULT 0,
    payload      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    result       JSONB,
    error        TEXT,
    scheduled_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS ai_jobs_ready_idx
    ON ai_jobs(status, scheduled_at)
    WHERE status IN ('queued','running');

-- Streams (live RTMP/WHIP — P4/P5) -----------------------------------------

CREATE TABLE IF NOT EXISTS streams (
    id           UUID        PRIMARY KEY,
    owner_id     UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id      UUID        REFERENCES rooms(id) ON DELETE SET NULL,
    title        TEXT        NOT NULL,
    stream_key   TEXT        NOT NULL UNIQUE,
    status       TEXT        NOT NULL DEFAULT 'idle'
                              CHECK (status IN ('idle','live','ended')),
    hls_path     TEXT,
    protocol     TEXT        NOT NULL DEFAULT 'rtmp'
                              CHECK (protocol IN ('rtmp','whip','srt')),
    started_at   TIMESTAMPTZ,
    ended_at     TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS streams_owner_idx ON streams(owner_id);
CREATE INDEX IF NOT EXISTS streams_live_idx ON streams(status) WHERE status = 'live';

-- Call sessions (P3 + P6 audit trail) --------------------------------------

CREATE TABLE IF NOT EXISTS call_sessions (
    id           UUID        PRIMARY KEY,
    room_id      UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    initiator    UUID        NOT NULL REFERENCES participants(id),
    kind         TEXT        NOT NULL CHECK (kind IN ('audio','video')),
    mode         TEXT        NOT NULL DEFAULT 'p2p'
                              CHECK (mode IN ('p2p','sfu')),
    started_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at     TIMESTAMPTZ,
    end_reason   TEXT
);

CREATE INDEX IF NOT EXISTS call_sessions_room_idx
    ON call_sessions(room_id, started_at DESC);

-- Call participants (per-leg state) ----------------------------------------

CREATE TABLE IF NOT EXISTS call_participants (
    call_id        UUID        NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    role           TEXT        NOT NULL CHECK (role IN ('caller','callee','member')),
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    left_at        TIMESTAMPTZ,
    PRIMARY KEY (call_id, participant_id)
);

CREATE INDEX IF NOT EXISTS call_participants_pid_idx ON call_participants(participant_id);
