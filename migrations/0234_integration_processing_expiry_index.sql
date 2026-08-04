-- The retention sweep expires abandoned processing rows only after both the
-- seven-day receipt window and their short fencing lease have elapsed. The
-- original expiry index deliberately excluded processing rows, which left
-- this recovery branch without an index as the request ledger grew.

CREATE INDEX IF NOT EXISTS integration_machine_requests_processing_expiry_idx
    ON integration_machine_requests
       (expires_at, lease_expires_at, installation_id, operation, idempotency_key)
    WHERE status = 'processing';
