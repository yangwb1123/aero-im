# aero-live-core

Dependency-light live-streaming contracts shared by all ingest backends. It
defines ingest configuration, the `LiveIngest` interface, and common stream
events so RTMP, SRT, and WHIP can use one server boot path.

Protocol handling and media conversion live in their dedicated live crates.

```bash
cargo test -p aero-live-core --lib
```
