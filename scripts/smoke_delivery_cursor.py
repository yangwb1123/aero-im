#!/usr/bin/env python3
"""Live smoke for ROADMAP 第六版 · 方向三·A — per-room persistent DELIVERY cursor.

Exercises the full path end-to-end against a running aero-server:
  1. B connects, A sends 5 messages; B records each frame's (seq, message.id).
  2. B sends `delivery_ack {room, message_id=msg3.id, seq=msg3.seq}` (durable
     receipt up to the 3rd message), then disconnects.
  3. GET /api/rooms/:id/delivery-cursor (as B) returns exactly msg3's (id, seq).
  4. B reconnects with `?cursors=1` (no explicit `since`): the per-room delivery
     cursor drives backfill, replaying ONLY msg4 & msg5 — never msg1..3.
  5. A second device of B reconnecting with `?cursors=1` sees the same post-cursor
     set (multi-device convergence — the shared per-room cursor, not per-device).
  6. The cursor endpoint is member-gated: a non-member (C) gets 403.
"""
import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:8099")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")
FAILS, OKS = [], []


def ok(m):
    OKS.append(m)
    print(f"  ✓ {m}")


def fail(m):
    FAILS.append(m)
    print(f"  ✗ {m}")


def req(method, path, token=None, body=None):
    headers = {}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    r = urllib.request.Request(HOST + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(r, timeout=12) as resp:
            t = resp.read().decode()
            return resp.status, (json.loads(t) if t else {})
    except urllib.error.HTTPError as e:
        t = e.read().decode()
        try:
            return e.code, (json.loads(t) if t else {})
        except Exception:
            return e.code, {"_raw": t}


def reg(label, ts):
    st, u = req(
        "POST",
        "/api/auth/register",
        body={"email": f"{label}_{ts}@aero.dev", "password": "password_123", "display_name": label},
    )
    if st not in (200, 201):
        sys.exit(f"register {label} failed: {st} {u}")
    return u


async def recv_until(ws, pred, timeout=6, max_frames=400):
    for _ in range(max_frames):
        try:
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        except asyncio.TimeoutError:
            return None
        if pred(f):
            return f
    return None


async def collect_messages(ws, want_ids, timeout=6, max_frames=400):
    """Collect `message` frames until every id in want_ids is seen (or timeout).
    Returns the set of message ids actually replayed."""
    seen = set()
    want = set(want_ids)
    for _ in range(max_frames):
        try:
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        except asyncio.TimeoutError:
            break
        if f.get("type") == "message":
            seen.add(f["message"]["id"])
            if want.issubset(seen):
                break
    return seen


async def main():
    ts = int(time.time())
    print("▶ register A, B, C")
    A = reg("dcA", ts)["access_token"]
    bobj = reg("dcB", ts)
    B = bobj["access_token"]
    bpid = bobj["participant"]["id"]
    C = reg("dcC", ts)["access_token"]

    print("▶ A creates room, adds B (not C)")
    st, room = req("POST", "/api/rooms", token=A, body={"kind": "group", "name": f"dc-{ts}"})
    if st not in (200, 201):
        sys.exit(f"create room failed: {st} {room}")
    rid = room["id"]
    req("POST", f"/api/rooms/{rid}/members", token=A, body={"participant_id": bpid})

    # ---- 1+2: B receives 5 messages live, ACKs delivery up to the 3rd ----
    print("▶ A sends 5 messages; B records (seq, id) and ACKs the 3rd")
    msgs = []  # (seq, id) in send order
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa, \
               websockets.connect(f"{WS_HOST}/ws?token={B}") as wb:
        assert json.loads(await wa.recv())["type"] == "welcome"
        assert json.loads(await wb.recv())["type"] == "welcome"
        await wa.send(json.dumps({"type": "join_room", "room_id": rid}))
        await wb.send(json.dumps({"type": "join_room", "room_id": rid}))
        await asyncio.sleep(0.4)  # let joins settle so B sees all fan-out
        for i in range(5):
            await wa.send(json.dumps({"type": "send_message", "room_id": rid,
                                      "blocks": [{"type": "text", "content": f"dc-msg-{i}"}]}))
        # B collects 5 message frames in order.
        while len(msgs) < 5:
            f = await recv_until(wb, lambda f: f.get("type") == "message", timeout=8)
            if f is None:
                break
            if isinstance(f.get("seq"), int):
                msgs.append((f["seq"], f["message"]["id"]))
        if len(msgs) == 5 and all(isinstance(s, int) for s, _ in msgs):
            ok(f"B received 5 live message frames with seq {[s for s, _ in msgs]}")
        else:
            fail(f"B did not receive 5 well-formed frames: {msgs}")
            return
        ack_seq, ack_id = msgs[2]  # the 3rd message
        await wb.send(json.dumps({"type": "delivery_ack", "room_id": rid,
                                  "message_id": ack_id, "seq": ack_seq}))
        await asyncio.sleep(0.6)  # best-effort async persist

    # ---- 3: REST endpoint returns exactly the ACKed cursor ----
    print("▶ GET /delivery-cursor reflects the ACK")
    cur = None
    for _ in range(5):
        st, cur = req("GET", f"/api/rooms/{rid}/delivery-cursor", token=B)
        if st == 200 and cur and cur.get("last_seq") == ack_seq:
            break
        await asyncio.sleep(0.3)
    if cur and cur.get("last_seq") == ack_seq and cur.get("last_delivered_message_id") == ack_id:
        ok(f"cursor endpoint returns last_seq={ack_seq}, id={str(ack_id)[:8]}…")
    else:
        fail(f"cursor endpoint mismatch: {st} {cur} (expected seq {ack_seq}, id {ack_id})")

    later_ids = {msgs[3][1], msgs[4][1]}  # msg4, msg5
    earlier_ids = {msgs[0][1], msgs[1][1], msgs[2][1]}

    # ---- 4: reconnect with ?cursors=1 replays ONLY post-cursor messages ----
    print("▶ B reconnects ?cursors=1 → backfill replays only msg4 & msg5")
    async with websockets.connect(f"{WS_HOST}/ws?token={B}&cursors=1") as wb2:
        assert json.loads(await wb2.recv())["type"] == "welcome"
        replayed = await collect_messages(wb2, later_ids, timeout=6)
    if later_ids.issubset(replayed) and not (replayed & earlier_ids):
        ok(f"per-room cursor backfill replayed exactly the 2 post-cursor msgs, skipped the 3 ACKed")
    else:
        fail(f"cursor backfill wrong: replayed={[str(i)[:8] for i in replayed]} "
             f"want⊇{[str(i)[:8] for i in later_ids]} and disjoint from ACKed")

    # ---- 5: a second device converges to the SAME shared cursor ----
    print("▶ second device of B (?cursors=1) sees the same post-cursor set")
    async with websockets.connect(f"{WS_HOST}/ws?token={B}&cursors=1") as wb3:
        assert json.loads(await wb3.recv())["type"] == "welcome"
        replayed2 = await collect_messages(wb3, later_ids, timeout=6)
    if later_ids.issubset(replayed2) and not (replayed2 & earlier_ids):
        ok("multi-device: 2nd device replays the shared per-room cursor (no per-device re-replay of ACKed)")
    else:
        fail(f"2nd device divergent: replayed={[str(i)[:8] for i in replayed2]}")

    # ---- 6: cursor endpoint is member-gated ----
    print("▶ non-member C is 403 on the cursor endpoint")
    st, _ = req("GET", f"/api/rooms/{rid}/delivery-cursor", token=C)
    if st == 403:
        ok("non-member gets 403 (assert_room_access)")
    else:
        fail(f"expected 403 for non-member, got {st}")

    print()
    print(f"RESULT: {len(OKS)} ok, {len(FAILS)} fail")
    if FAILS:
        for f in FAILS:
            print(f"  FAIL: {f}")
        sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
