# aero-im-call

One-to-one and group-call lifecycle orchestration. `CallOrchestrator` persists
start, answer, join, leave, end, and missed-call state, maintains local SFU peer
bookkeeping, and derives cross-node bridge intent from the route registry.

WebSocket signaling and media transport remain in `aero-server` and
`aero-live-webrtc` respectively.

```bash
cargo test -p aero-im-call --lib
```
