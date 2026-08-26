# aero-live-hls

HLS media utilities and durable segment writer. It owns MPEG-TS muxing,
FLV-to-TS conversion, atomic segment persistence, and rolling `index.m3u8`
generation under each stream directory.

The crate is ingest-agnostic and is reused by RTMP, WHIP, and SRT pipelines.

```bash
cargo test -p aero-live-hls --lib
```
