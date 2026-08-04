#!/usr/bin/env python3
"""Live smoke for canonical call-signaling authorization.

Every existing-call mutation resolves the persisted call by ``call_id``, verifies
that the frame's ``room_id`` matches its canonical room, and requires the
authenticated sender (plus any directed recipient) to be an active call
participant.

  - A owns a room, C is a member/callee, B is a NON-member.
  - A starts a real persisted call and C answers it.
  - B's spoofed CallEnd/CallIce receives an error frame, while C receives no
    spoofed event.
  - C's legitimate ICE and CallEnd reach A, proving the canonical guard preserves
    valid signaling.
"""
import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request

import websockets

B = os.environ.get("AERO_HOST", "http://localhost:3030").rstrip("/")
if B.startswith("https://"):
    W = f"wss://{B.removeprefix('https://')}"
elif B.startswith("http://"):
    W = f"ws://{B.removeprefix('http://')}"
else:
    raise ValueError("AERO_HOST must start with http:// or https://")


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
    apid = a["participant"]["id"]
    rid = call("POST", "/api/rooms", ta, {"kind": "group", "name": f"idor-{int(time.time())}"})["id"]
    call("POST", f"/api/rooms/{rid}/members", ta, {"participant_id": c["participant"]["id"]})  # add C; B excluded
    fails = []
    async with websockets.connect(f"{W}/ws?token={ta}") as wa, \
               websockets.connect(f"{W}/ws?token={tc}") as wc, \
               websockets.connect(f"{W}/ws?token={tb}") as wb:
        for ws in (wa, wc, wb):
            assert json.loads(await ws.recv())["type"] == "welcome"
        await wa.send(json.dumps({"type": "join_room", "room_id": rid}))
        await wc.send(json.dumps({"type": "join_room", "room_id": rid}))
        await asyncio.sleep(0.3)

        sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n"
        await wa.send(json.dumps({
            "type": "call_invite",
            "room_id": rid,
            "kind": "audio",
            "sdp": sdp,
        }))
        invite = await recv_until(
            wc,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "invite",
        )
        if invite is None:
            fails.append("member C did not receive the canonical call invite")
            cid = None
        else:
            cid = invite["event"].get("call_id")
            if not cid:
                fails.append(f"canonical invite omitted call_id: {invite}")
        if cid is None:
            for failure in fails:
                print("FAIL:", failure)
            sys.exit(1)

        await wc.send(json.dumps({
            "type": "call_answer",
            "call_id": cid,
            "room_id": rid,
            "to": apid,
            "sdp": sdp,
        }))
        answer = await recv_until(
            wa,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "answer"
            and f.get("event", {}).get("call_id") == cid,
        )
        if answer is None:
            fails.append("member C's canonical answer did not reach A")

        await wb.send(json.dumps({"type": "call_end", "call_id": cid, "room_id": rid, "reason": "hijack"}))
        berr = await recv_until(wb, lambda f: f.get("type") == "error", timeout=3)
        cleak = await recv_until(
            wc,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "end"
            and f.get("event", {}).get("call_id") == cid,
            timeout=2,
        )
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
        ice_leak = await recv_until(
            wc,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "ice"
            and f.get("event", {}).get("from") == b["participant"]["id"],
            timeout=2,
        )
        if ice_leak is not None:
            fails.append(f"member received spoofed ICE: {ice_leak}")

        await wc.send(json.dumps({
            "type": "call_ice",
            "call_id": cid,
            "room_id": rid,
            "to": apid,
            "candidate": {
                "candidate": "candidate:2 1 udp 1 127.0.0.1 9 typ host",
                "sdpMid": "0",
                "sdpMLineIndex": 0,
            },
        }))
        legitimate_ice = await recv_until(
            wa,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "ice"
            and f.get("event", {}).get("call_id") == cid
            and f.get("event", {}).get("from") == c["participant"]["id"],
        )
        if legitimate_ice is None:
            fails.append("member C's canonical ICE did not reach A")

        await wc.send(json.dumps({"type": "call_end", "call_id": cid, "room_id": rid, "reason": "bye"}))
        legitimate_end = await recv_until(
            wa,
            lambda f: f.get("type") == "call"
            and f.get("event", {}).get("op") == "end"
            and f.get("event", {}).get("call_id") == cid,
        )
        if legitimate_end is None:
            fails.append("member C's canonical CallEnd did not reach A")

    if fails:
        for f in fails:
            print("FAIL:", f)
        sys.exit(1)
    print("RESULT: canonical call authorization VERIFIED")


if __name__ == "__main__":
    asyncio.run(main())
