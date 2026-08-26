# aero-signaling

Shared WebRTC signaling types and validation for calls. It parses and bounds
SDP/ICE payloads, builds browser RTC/ICE configuration, and provides the small
in-memory signaling roster used by the WS mesh path.

It does not relay events or carry media. NATS relay lives in `aero-im-core` and
`aero-server`; SFU media lives in `aero-live-webrtc`.

```bash
cargo test -p aero-signaling --lib
```
