-- Join requests are tenant-contained through their room and must reference real
-- identities. Legacy orphan/corrupt rows cannot be authorized, so remove them
-- before adding the missing integrity constraints.

DELETE FROM channel_join_requests request
 WHERE request.status NOT IN ('pending', 'approved', 'denied')
    OR NOT EXISTS (
        SELECT 1 FROM rooms room WHERE room.id = request.room_id
    )
    OR NOT EXISTS (
        SELECT 1 FROM participants participant
         WHERE participant.id = request.requester_id
    );

UPDATE channel_join_requests
   SET decided_at = NULL,
       decided_by = NULL
 WHERE status = 'pending';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_join_requests_room_id_fkey'
           AND conrelid = 'channel_join_requests'::regclass
    ) THEN
        ALTER TABLE channel_join_requests
            ADD CONSTRAINT channel_join_requests_room_id_fkey
            FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_join_requests_requester_id_fkey'
           AND conrelid = 'channel_join_requests'::regclass
    ) THEN
        ALTER TABLE channel_join_requests
            ADD CONSTRAINT channel_join_requests_requester_id_fkey
            FOREIGN KEY (requester_id) REFERENCES participants(id) ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_join_requests_decided_by_fkey'
           AND conrelid = 'channel_join_requests'::regclass
    ) THEN
        ALTER TABLE channel_join_requests
            ADD CONSTRAINT channel_join_requests_decided_by_fkey
            FOREIGN KEY (decided_by) REFERENCES participants(id) ON DELETE SET NULL;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_join_requests_status_check'
           AND conrelid = 'channel_join_requests'::regclass
    ) THEN
        ALTER TABLE channel_join_requests
            ADD CONSTRAINT channel_join_requests_status_check
            CHECK (status IN ('pending', 'approved', 'denied'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'channel_join_requests_pending_decision_check'
           AND conrelid = 'channel_join_requests'::regclass
    ) THEN
        ALTER TABLE channel_join_requests
            ADD CONSTRAINT channel_join_requests_pending_decision_check
            CHECK (
                status <> 'pending'
                OR (decided_at IS NULL AND decided_by IS NULL)
            );
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS channel_join_requests_requester_idx
    ON channel_join_requests (requester_id, status, created_at DESC);
