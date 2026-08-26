# aero-im-core

Core IM business orchestration. `ImService` applies membership, validation,
moderation, message lifecycle, notification, audit, outbox, and call-signaling
invariants over the storage and event-bus crates.

Transport-specific HTTP and WebSocket handling stays in `aero-server`. Room
data entry points must pass through the canonical effective-access guard.

```bash
cargo test -p aero-im-core --lib
```

Database-gated tests require a freshly migrated disposable PostgreSQL database.
