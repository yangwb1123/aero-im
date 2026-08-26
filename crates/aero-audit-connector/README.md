# aero-audit-connector

Leased, fenced relay from Aero's durable governance audit outbox to an external
audit receiver. It implements priority-aware claims, heartbeat fencing,
credential validation, retry/backoff, permanent dead-letter classification,
metrics, PostgreSQL and in-memory repositories, and operational drill binaries.

No external delivery is acknowledged until the fenced settlement transaction
commits.

```bash
cargo test -p aero-audit-connector --lib
```
