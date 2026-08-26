# aero-live-srt

SRT HSv5 ingest and MPEG-TS segmentation. The crate implements handshake and
stream-id parsing, AES-CTR media protection, RFC 3394 key wrapping, even/odd SEK
rotation, ACK/NAK reliability, reordering, and H.264 keyframe-aligned HLS output.

It also exposes short-lived TURN REST credential helpers used by the live boot
path.

```bash
cargo test -p aero-live-srt --lib
```
