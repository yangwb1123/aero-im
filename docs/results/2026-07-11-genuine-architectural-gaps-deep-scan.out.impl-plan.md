Done. The implementation plan has been saved to `docs/plans/2026-07-12-four-systemic-gaps-implementation-plan.md`.

Here's a summary of what the plan covers:

## Plan Overview

**4 directions, 35 tasks, ~118 engineer-hours, 4-week execution** with a 2-person team (1 backend + 1 frontend/SDET).

### Direction A: Canvas CRDT Real-time Collaboration (51h, 12 tasks)

Takes the existing Canvas system (which already has CRUD + op log + REST API) and wires it for real-time:
- `RoomEvent::CanvasOp` → NATS bus → WS fan-out (A-01~A-03)
- Yjs CRDT engine in the web SPA (A-06~A-07)
- Real-time collaborative editing (A-08)
- Reconnect op backfill via `?since_op_seq=` (A-04)
- Op log GC + read-time snapshot (A-10, A-12)

### Direction B: Orphan Data Cleanup (18h, 8 tasks)

Targets the soft-delete orphan gap where `sweep_expired_messages` (UPDATE) leaves reactions/notifications/pins/receipts pointing to zombie messages:
- Data analysis query (B-01)
- Three migration files with 7-day grace window (B-02~B-04)
- Daily orphan sweep timer with legal-hold exemption (B-05~B-06)
- Prometheus metrics (B-08)

### Direction C: Reconnection State Events (26h, 8 tasks)

Extends the current `backfill_since` (messages-only) to include room state:
- `RoomStateSnapshot` model + `RoomStateRepo` (C-02~C-03)
- `?include_state=1` WS parameter (C-04)
- Web SPA consumption of pinned messages, polls, roster (C-05)
- Performance testing for 10K-member rooms (C-07)

### Direction D: Fail-open Testing (23h, 7 tasks)

Addresses the zero-test-coverage for all fail-open paths:
- Audit inventory of all 7+ fail-open decision points (D-01)
- Mock/fault-injection tests for WsRate, LoginThrottle, SpamGuard, NotifPrefs (D-02~D-05)
- Generic fault-injection framework (D-06)
- Runtime status endpoint + documentation (D-07)

### Key Risks Highlighted

1. **Canvas**: Yjs ↔ custom op format encoding complexity → mitigated by keeping server agnostic to op content
2. **Orphan**: Large DELETE volume → `LIMIT 10000` + configurable grace window
3. **Reconnect**: Large room state snapshots → capped fields + "more available" truncation signal
4. **Fail-open**: WsRateStore is a struct not a trait → extraction cost evaluated before committing
