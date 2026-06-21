#!/usr/bin/env python3
"""Live smoke for the call-signaling IDOR fix (gap-scan #2-6).

A non-member must NOT be able to inject WebSocket call signaling (CallEnd / CallIce
/ …) into a room they don't belong to by spoofing room_id. relay_call_event now
gates on is_member(room, authenticated_pid).

  - A owns a room, C is a member, B is a NON-member.
  - B's spoofed CallEnd/CallIce → 'forbidden: not a room member' error frame, and
    member C receives NO spoofed call event (no leak).
  - Member C's own call event is NOT rejected for membership (legitimate use intact).
"""
import asyncio
import json
import sys
import time
import urllib.error
import urllib.request

import websockets

B = "http://127.0.0.1:8099"
W = "ws://127.0.0.1:8099"


def call(method, path, tok=None, body=None):
    h = {}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["content-type"] = "application/json"
    if tok:
        h["authorization"] = f"Bearer {tok}"
    r = urllib.request.Request(B + path, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=10) as resp:
            t = resp.read().decode()
    except urllib.error.HTTPError as e:
        t = e.read().decode()
    return json.loads(t) if t.strip() else {}


def reg(label):
    return call("POST", "/api/auth/register", body={
        "email": f"{label}_{int(time.time()*1000)}@aero.dev",
        "password": "password_123", "display_name": label})


async def recv_until(ws, pred, timeout=3):
    try:
        for _ in range(60):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
            if pred(f):
                return f
    except asyncio.TimeoutError:
        return None
    return None


async def main():
    a, c, b = reg("idorA"), reg("idorC"), reg("idorB")
    ta, tc, tb = a["access_token"], c["access_token"], b["access_token"]
    rid = call("POST", "/api/rooms", ta, {"kind": "group", "name": f"idor-{int(time.time())}"})["id"]
    call("POST", f"/api/rooms/{rid}/members", ta, {"participant_id": c["participant"]["id"]})  # add C; B excluded
    cid = "01JBKHQK3M8XQZ9F6Y7N2P4R5T"  # valid ULID
    fails = []
    async with websockets.connect(f"{W}/ws?token={tc}") as wc, \
               websockets.connect(f"{W}/ws?token={tb}") as wb:
        assert json.loads(await wc.recv())["type"] == "welcome"
        assert json.loads(await wb.recv())["type"] == "welcome"
        await wc.send(json.dumps({"type": "join_room", "room_id": rid}))
        await asyncio.sleep(0.3)

        await wb.send(json.dumps({"type": "call_end", "call_id": cid, "room_id": rid, "reason": "hijack"}))
        berr = await recv_until(wb, lambda f: f.get("type") == "error", timeout=3)
        cleak = await recv_until(wc, lambda f: f.get("type") not in ("presence", "welcome") and "call" in json.dumps(f).lower(), timeout=2)
        if berr is None:
            fails.append("non-member CallEnd was not rejected")
        if cleak is not None:
            fails.append(f"member received a spoofed call event: {cleak}")
        print(f"non-member CallEnd → {berr}")

        await wb.send(json.dumps({"type": "call_ice", "call_id": cid, "room_id": rid,
                                  "to": c["participant"]["id"],
                                  "candidate": {"candidate": "candidate:1 1 udp 1 1.1.1.1 9 typ host", "sdpMid": "0", "sdpMLineIndex": 0}}))
        berr2 = await recv_until(wb, lambda f: f.get("type") == "error", timeout=3)
        if berr2 is None:
            fails.append("non-member CallIce was not rejected")

        await wc.send(json.dumps({"type": "call_end", "call_id": cid, "room_id": rid, "reason": "bye"}))
        cerr = await recv_until(wc, lambda f: f.get("type") == "error" and "member" in json.dumps(f).lower(), timeout=2)
        if cerr is not None:
            fails.append(f"member's own call event wrongly rejected: {cerr}")

    if fails:
        for f in fails:
            print("FAIL:", f)
        sys.exit(1)
    print("RESULT: call IDOR fix VERIFIED")


if __name__ == "__main__":
    asyncio.run(main())
