-- 0155_canvas_ops.sql — append-only collaborative op log for channel canvases
-- (ROADMAP 第六版 · 方向五·③ · 实时协作原语).
--
-- The canvas body today is a full-document `blocks` JSONB replaced wholesale by
-- PUT (now guarded by optimistic `version`, mig 0152 — a concurrent edit gets a
-- 409 rather than silently losing). That prevents lost UPDATES but still forces
-- editors to serialize: two people typing in different paragraphs conflict. The
-- modern-workspace primitive is an APPEND-ONLY op log — each edit is an immutable
-- incremental op (insert/delete/format) that clients reduce locally (OT/CRDT), so
-- concurrent edits compose instead of clobbering.
--
-- This adds that durable, totally-ordered-per-canvas op log. `op_seq` on the
-- canvas is the monotonic counter; `append` row-locks the canvas to bump it, so
-- every op gets a distinct, gap-free `seq` even under concurrent appends. The
-- existing `blocks`/`version` full-document model is untouched (a server-side
-- snapshot/materialization from the op log is a later step) — purely additive.

ALTER TABLE channel_canvases
    ADD COLUMN IF NOT EXISTS op_seq BIGINT NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS canvas_ops (
    id         UUID        PRIMARY KEY,
    canvas_id  UUID        NOT NULL REFERENCES channel_canvases(id) ON DELETE CASCADE,
    -- Per-canvas monotonic, gap-free position (assigned under the canvas row lock).
    seq        BIGINT      NOT NULL,
    author_id  UUID        NOT NULL,
    -- The incremental edit op, stored verbatim (client-defined shape: insert /
    -- delete / format / …). The server is an ordered, immutable log — it does not
    -- interpret op semantics; clients reduce the stream.
    op         JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (canvas_id, seq)
);

-- Delta fetch: "ops for this canvas after seq N, in order".
CREATE INDEX IF NOT EXISTS canvas_ops_canvas_seq_idx ON canvas_ops (canvas_id, seq);
