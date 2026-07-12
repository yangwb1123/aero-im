Your analysis is **validated** — I've traced every code-level claim against the actual source, and the findings are correct across all 5 directions. Let me give you a succinct code-verified judgment on each, plus some nuance on points the document gets slightly wrong.

---

## ✅ Verified Claims Summary

| # | Direction | Code-verified? | Nuance/Correction |
|---|-----------|---------------|-------------------|
| 1 | **PWA / Offline** | ✅ **Confirmed** | `web/ws.js:168` `send()` returns false → silent loss. Zero SW/cache/PWA elements in `web/`. No IndexedDB usage for anything but JWT token in localStorage (`web/api.js`). Zero grep hits across 337 requirements docs. |
| 2 | **Build Pipeline** | ✅ **Confirmed** | `web/package.json` has only ESLint. 17 JS files loaded via `<script type="module" src="...">` — each triggers an independent HTTP request. |
| 3 | **State Continuity** | ✅ **Confirmed** | `web/context.js` exports `export const state = { me: null, rooms: new Map(), currentRoomId: null, ... }` — pure in-memory. No `saveDraft`, no `beforeunload`, no `sessionStorage` usage beyond auth. |
| 4 | **E2E Latency Vacuum** | ✅ **Largely Confirmed** | `bus.rs` `handle_room_event_sub` has zero timing. `hub.rs` `fan_out_raw` records nothing on `try_send`. NATS backlog only monitors **2 of 9** consumers (`CONSUMERS: &[("IM_MESSAGES","aero-server"), ("AI_QUEUE","aero-ai")]` in `metrics_tasks.rs`). **Minor correction**: `aero_message_processing_duration_seconds` exists in `aero_im_core` (message mutate), and `MESSAGES_SENT_TOTAL` is indeed registered in `aero-common/src/metrics.rs` — the document claims it's missing, but it exists; what's missing are the per-phase separation and the WS→client rendering time. |
| 5 | **Blob Distribution** | ✅ **Confirmed** | `blob_download` (routes.rs ~1752): no `Cache-Control`, no `ETag`, no `Range`/`206` support, no compression. Full bytes read into memory every request. Thumbnails/`image` crate not imported anywhere. |

---

## Additional Diagnostics Found During Validation

Some things your analysis didn't catch that strengthen or constrain the directions:

**Direction 1** — `web/ws.js:_scheduleReconnect()` implements exponential backoff reconnect already (includes jitter, `_seqGate.reset()` on reconnect). This means the **offline reconnect path is wired** — the only gap is the missing offline message queue (the wire is ready, the buffer isn't).

**Direction 3** — The codebase has an existing ROADMAP comment in `context.js`:
```javascript
// ROADMAP v3 方向一: last-applied edit timestamp per message id
```
This confirms state persistence was already identified as a roadmap item internally. The analysis is correct that nothing has been implemented.

**Direction 4** — The bus listener (`bus.rs`) already has W3C traceparent propagation:
```rust
fn bus_consume_span(subject: &'static str, payload: &[u8]) -> tracing::Span {
    // extracts traceparent from bus envelope
}
```
This means **distributed tracing across NATS** is partially wired — the span propagation exists. The gap is no timing metrics (histograms) across the phases, not the tracing context.

**Direction 5** — The blob upload pipeline is actually quite sophisticated (ClamAV, content sniffing, SHA256 dedup, IDOR guard). The download path is the orphan — all the investment went into upload security, none into delivery optimization.

---

## Prioritization Assessment

Your Sprint N proposal is sound for all 5 directions. One re-prioritization suggestion:

| Item | You said | I'd say | Rationale |
|------|----------|---------|-----------|
| **Direction 4** (E2E latency) | Sprint N+1 | **Move to Sprint N** | The tracing infrastructure (`traceparent` in bus envelope) already exists but is unused by metrics. Adding timing to `handle_room_event_sub` and `fan_out_raw` is ~50 lines and would close the largest production blind spot **immediately**. The NATS consumer backlog expansion from 2→9 consumers is also a one-line constant array change. |

The E2E latency gap is the **cheapest to fix** relative to impact — it requires no new dependencies, no architectural changes, just adding `Instant::now()` measurements at 5 points in existing code and emitting histograms.

---

## Next Steps

If you want, I can:
1. **Save this analysis** as `docs/requirements/2026-07-12-five-systemically-overlooked-high-value-extensions.md`
2. **Implement the Sprint N P0 items** (SW + esbuild + sessionStorage + Cache-Control)
3. **Discuss any direction in more depth** with code-level implementation sketches

What's your priority?
