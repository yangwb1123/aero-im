#!/usr/bin/env python3
"""Live smoke for ROADMAP 第六版 · 方向三·A — per-room persistent DELIVERY cursor.

Exercises the full path end-to-end against a running aero-server:
  1. B connects, A sends 5 messages; B records each frame's
     (delivery_ordinal, seq, message.id).
  2. B sends `delivery_ack` with msg3's exact ordinal/id/seq (durable receipt up
     to the 3rd message), then disconnects.
  3. GET /api/rooms/:id/delivery-cursor returns msg3's ordinal/id/seq.
  4. A writes more than one 200-row replay page while B is offline. B reconnects
     with `?cursors=1`; every post-cursor row is replayed before `delivery_ready`,
     followed by a live message (the FIFO barrier).
  5. B ACKs the live tail and the REST cursor advances. A second device then
     receives no old replay before `delivery_ready`, proving the shared
     participant/room max cursor converges across devices.
  6. The cursor endpoint is member-gated: a non-member (C) gets 403.
"""
import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:8099")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")
FAILS, OKS = [], []
PAGED_REPLAY_COUNT = 205


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


async def send_with_ack(ws, room_id, content, timeout=10):
    """Persist one message and return its server-assigned id."""
    client_id = str(uuid.uuid4())
    await ws.send(json.dumps({
        "type": "send_message",
        "room_id": room_id,
        "blocks": [{"type": "text", "content": content}],
        "client_message_id": client_id,
    }))
    ack = await recv_until(
        ws,
        lambda frame: (
            frame.get("type") == "message_ack"
            and frame.get("client_message_id") == client_id
        ),
        timeout=timeout,
    )
    if ack is None or not isinstance(ack.get("message"), dict):
        raise RuntimeError(f"missing message_ack for {client_id}")
    return ack["message"]["id"]


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
    print("▶ A sends 5 messages; B records (ordinal, seq, id) and ACKs the 3rd")
    msgs = []  # (delivery_ordinal, seq, id) in send order
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
            if isinstance(f.get("seq"), int) and isinstance(f.get("delivery_ordinal"), int):
                msgs.append((f["delivery_ordinal"], f["seq"], f["message"]["id"]))
        if len(msgs) == 5:
            ok(f"B received ordinals {[o for o, _, _ in msgs]}")
        else:
            fail(f"B did not receive 5 well-formed frames: {msgs}")
            return
        ack_ordinal, ack_seq, ack_id = msgs[2]  # the 3rd message
        await wb.send(json.dumps({"type": "delivery_ack", "room_id": rid,
                                  "message_id": ack_id,
                                  "delivery_ordinal": ack_ordinal,
                                  "seq": ack_seq}))
        await asyncio.sleep(0.6)  # best-effort async persist

    # ---- 3: REST endpoint returns exactly the ACKed cursor ----
    print("▶ GET /delivery-cursor reflects the ACK")
    cur = None
    for _ in range(5):
        st, cur = req("GET", f"/api/rooms/{rid}/delivery-cursor", token=B)
        if (st == 200 and cur and cur.get("last_seq") == ack_seq
                and cur.get("last_delivery_ordinal") == ack_ordinal):
            break
        await asyncio.sleep(0.3)
    if (cur and cur.get("last_seq") == ack_seq
            and cur.get("last_delivery_ordinal") == ack_ordinal
            and cur.get("last_delivered_message_id") == ack_id):
        ok(f"cursor returns ordinal={ack_ordinal}, seq={ack_seq}, id={str(ack_id)[:8]}…")
    else:
        fail(f"cursor endpoint mismatch: {st} {cur}")

    later_ids = {msgs[3][2], msgs[4][2]}  # msg4, msg5
    earlier_ids = {msgs[0][2], msgs[1][2], msgs[2][2]}

    # ---- 4: exercise >1 page and the replay/live DeliveryReady barrier ----
    print(f"▶ A writes {PAGED_REPLAY_COUNT} messages while B is offline")
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa2:
        assert json.loads(await wa2.recv())["type"] == "welcome"
        await wa2.send(json.dumps({"type": "join_room", "room_id": rid}))
        await asyncio.sleep(0.2)
        for i in range(PAGED_REPLAY_COUNT):
            later_ids.add(await send_with_ack(wa2, rid, f"dc-page-{i}"))
        ok(f"offline tail spans >200 rows ({len(later_ids)} total post-cursor)")

        print("▶ B reconnects → complete replay, DeliveryReady, then live frame")
        async with websockets.connect(f"{WS_HOST}/ws?token={B}&cursors=1") as wb2:
            assert json.loads(await wb2.recv())["type"] == "welcome"

            # Consume the first replay row, then create a live row while the
            # reconnect stream is still applying its multi-page tail.
            first = await recv_until(wb2, lambda frame: frame.get("type") == "message", timeout=8)
            if first is None:
                fail("cursor reconnect produced no first replay row")
                return
            replayed = {first["message"]["id"]}
            frame_index = 1
            ready_index = None
            live_after_ready = None
            live_id = await send_with_ack(wa2, rid, "dc-live-during-replay")

            # A live event committed during replay is held on the Hub queue until
            # DeliveryReady. It can also be visible in the DB replay snapshot;
            # require at least one copy after the barrier and never ACK before it.
            for _ in range(900):
                try:
                    frame = json.loads(await asyncio.wait_for(wb2.recv(), timeout=10))
                except asyncio.TimeoutError:
                    break
                frame_index += 1
                if frame.get("type") == "delivery_ready":
                    ready_index = frame_index
                    continue
                if frame.get("type") != "message":
                    continue
                message_id = frame["message"]["id"]
                if ready_index is None:
                    replayed.add(message_id)
                elif message_id == live_id:
                    live_after_ready = frame
                    break

            replay_ok = later_ids.issubset(replayed) and not (replayed & earlier_ids)
            if replay_ok:
                ok("cursor replay crossed the 200-row page boundary without a gap")
            else:
                missing = later_ids - replayed
                fail(
                    "cursor backfill mismatch: "
                    f"missing={len(missing)}, leaked_acked={len(replayed & earlier_ids)}"
                )
            if ready_index is not None and live_after_ready is not None:
                ok("DeliveryReady is a strict FIFO barrier before the buffered live frame")
            else:
                fail(
                    "missing DeliveryReady/live FIFO evidence: "
                    f"ready={ready_index is not None}, live_after={live_after_ready is not None}"
                )
                return

            # ACK only after applying the post-barrier live frame.
            live_ordinal = live_after_ready.get("delivery_ordinal")
            live_seq = live_after_ready.get("seq")
            if not isinstance(live_ordinal, int) or not isinstance(live_seq, int):
                fail(f"live frame lacks cursor tuple: {live_after_ready}")
                return
            await wb2.send(json.dumps({
                "type": "delivery_ack",
                "room_id": rid,
                "message_id": live_id,
                "delivery_ordinal": live_ordinal,
                "seq": live_seq,
            }))

    # Persist is asynchronous; wait until the shared cursor reaches the live tail.
    live_cursor = None
    for _ in range(20):
        st, live_cursor = req("GET", f"/api/rooms/{rid}/delivery-cursor", token=B)
        if (
            st == 200
            and live_cursor.get("last_delivered_message_id") == live_id
            and live_cursor.get("last_delivery_ordinal") == live_ordinal
        ):
            break
        await asyncio.sleep(0.2)
    if (
        live_cursor
        and live_cursor.get("last_delivered_message_id") == live_id
        and live_cursor.get("last_delivery_ordinal") == live_ordinal
    ):
        ok("device A advanced the shared participant/room cursor to the live tail")
    else:
        fail(f"shared cursor did not advance to live tail: {st} {live_cursor}")

    # ---- 5: a second device starts after the shared max cursor ----
    print("▶ second device reconnects after device A ACK → no old replay")
    pre_ready_messages = set()
    async with websockets.connect(f"{WS_HOST}/ws?token={B}&cursors=1") as wb3:
        assert json.loads(await wb3.recv())["type"] == "welcome"
        ready2 = None
        for _ in range(50):
            try:
                frame = json.loads(await asyncio.wait_for(wb3.recv(), timeout=4))
            except asyncio.TimeoutError:
                break
            if frame.get("type") == "delivery_ready":
                ready2 = frame
                break
            if frame.get("type") == "message":
                pre_ready_messages.add(frame["message"]["id"])
        if ready2 is not None:
            # A short post-barrier quiet period catches delayed stale replay.
            try:
                while True:
                    frame = json.loads(await asyncio.wait_for(wb3.recv(), timeout=0.8))
                    if frame.get("type") == "message":
                        pre_ready_messages.add(frame["message"]["id"])
            except asyncio.TimeoutError:
                pass
    if ready2 is not None and not pre_ready_messages:
        ok("multi-device convergence: second device starts at the shared max cursor")
    else:
        fail(
            "second device replayed acknowledged rows or lacked DeliveryReady: "
            f"ready={ready2 is not None}, messages={len(pre_ready_messages)}"
        )

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
