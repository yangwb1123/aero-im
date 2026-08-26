# aero-server

Composition layer and network gateway for Aero IM. It assembles Axum HTTP and
WebSocket routes, authentication, repositories, NATS listeners, bots, workers,
timers, live ingest, HLS serving, SFU sessions, and cross-node call bridges into
the production `aero-server` binary.

The same package also builds the database-capable `aero-cli`; shared engineering
gate behavior is delegated to `aero-eng`.

```bash
cargo test -p aero-server --lib
```

External-service and browser/media tests are explicitly gated for disposable
integration or staging environments.
