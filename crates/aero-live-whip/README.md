# aero-live-whip

str0m-based WHIP/WHEP media plane. It creates real SDP answers, drives
ICE/DTLS/SRTP sessions, depacketizes RFC 6184 H.264 RTP, relays access units,
and converts received media into the shared HLS pipeline.

Hermetic tests cover negotiation, packetization, depacketization, and RTP-to-HLS
bytes. Physical devices and public-network ICE/TURN remain staging validation.

```bash
cargo test -p aero-live-whip --lib
```
