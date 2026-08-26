# aero-common

Shared, dependency-leaf contracts for Aero IM. This crate owns strongly typed
IDs, cross-crate models and events, configuration, errors, telemetry, and the
lightweight Prometheus-compatible metrics registry.

It intentionally contains no persistence or service orchestration. MLS values
are opaque transport/storage scaffolding; the client-side MLS state machine is
outside this repository's scope.

```bash
cargo test -p aero-common --lib
```
