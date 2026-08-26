# aero-bus

NATS JetStream transport for Aero IM's event DAG. It provides the object-safe
`EventBus` boundary, the production `JetStreamBus`, durable and ephemeral
consumer setup, subject validation, idempotent publication, and per-subject
sequence stamping.

Business fan-out remains in `aero-server`; this crate only owns broker-facing
delivery semantics.

```bash
cargo test -p aero-bus --lib
```
