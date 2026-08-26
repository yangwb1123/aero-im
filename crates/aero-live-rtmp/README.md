# aero-live-rtmp

RTMP publishing endpoint backed by `rml_rtmp`. It authenticates stream keys,
tracks the live lifecycle, converts incoming FLV H.264/AAC tags into MPEG-TS,
and feeds the shared HLS writer.

The server owns listener boot and persistent stream metadata; segment layout
and muxing utilities are supplied by `aero-live-hls`.

```bash
cargo test -p aero-live-rtmp --lib
```
