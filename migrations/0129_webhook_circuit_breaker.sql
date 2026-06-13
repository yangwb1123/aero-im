-- Per-endpoint circuit-breaker state for outgoing webhooks (ROADMAP5 方向一).
--
-- Before this, a receiver that was down for an hour got POSTed on every matching
-- room event — thousands of doomed deliveries/hour, each burning a reqwest
-- connection + a DLQ row (webhooks.rs evidence: only per-delivery backoff, no
-- per-endpoint breaker). These two columns let the dispatcher trip a breaker after
-- N consecutive failures (or immediately on a 429), skip the endpoint until the
-- cooldown elapses, then send a single half-open probe that re-opens or closes it.
--
-- `breaker_failures`   running count of consecutive failures; reset to 0 on any 2xx.
-- `breaker_open_until` while in the future, the dispatcher SKIPS this endpoint.
--                      NULL = closed (deliver normally).
ALTER TABLE outgoing_webhooks
    ADD COLUMN IF NOT EXISTS breaker_failures   INTEGER     NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS breaker_open_until TIMESTAMPTZ;
