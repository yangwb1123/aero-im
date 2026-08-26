# aero-live-webrtc

Pure-Rust str0m selective forwarding unit for group calls and interactive live
media. It owns peer negotiation, track routing, RTP sequence/timestamp remap,
simulcast selection, RTCP feedback, congestion estimates, and cross-node call
bridge media primitives.

The server owns sockets and task lifecycles. Public-network NAT, TURN, Safari,
and physical-device coverage remain staging concerns.

```bash
cargo test -p aero-live-webrtc --lib
```
