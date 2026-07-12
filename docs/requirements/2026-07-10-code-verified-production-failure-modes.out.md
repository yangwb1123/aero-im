The file `2026-07-10-code-verified-production-failure-modes.md` already exists at `/home/u1/aero-im/docs/requirements/` with the exact same content you pasted. I've verified the key code references:

- ✅ **方向一 (AI 路由)**: `routes.rs` lines 117-120, 223, 330, 345, 384 match
- ✅ **方向二 (HLS 无认证)**: `serve.rs` line 111 has `.nest_service("/hls", ServeDir::new(&hls_dir))`
- ✅ **方向三 (ws:// 降级)**: `web/ws.js:92` — `const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'`
- ✅ **方向四 (slow-consumer resync)**: `hub.rs:44` defined `RESYNC_FRAME`, `fan_out_arc_inner` at line ~310 has the `lossy`/`try_send` drop logic matching the description
- ✅ **方向五 (无 ack)**: `frame.rs:565` `send_blocks_frame` returns `Ok(())` at the end with no ack frame sent back

One correction: the document mentions **Hub** in `crates/aero-bus/src/hub.rs`, but the actual implementation with the resync/drop logic is in **`crates/aero-server/src/hub.rs`** (the `aero-bus` crate has no hub.rs). The code references and analysis are otherwise accurate.

Did you want me to read or verify something further about this analysis, or help implement any of the 5 fixes described?

Looking at the document's stated intent —「可直接转化为 GitHub Issue 或 Sprint Task」— I can help create implementation PRs if that's the next step.
