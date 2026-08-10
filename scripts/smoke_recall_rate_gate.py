#!/usr/bin/env python3
"""Live smoke for the recall rate gate (F3) + role-gated budget conservation (S1).

Verifies, against a booted server, that the recall rate gate behaves as the
security review demands:

  1. Budget conservation (gate S1): a PLAIN room member's doomed recall
     attempts on another member's message are Forbidden from the preflight
     (`assert_message_recall_preflight` carries the role gate) and consume
     ZERO workspace budget. Proved behaviorally: with a tiny standard-tier
     budget, the author's OWN recall still succeeds afterwards — had the
     member been charged, the budget would already be exhausted.
  2. REST rate-fire: an authorized caller over the workspace window gets
     HTTP 429 with a Retry-After header.
  3. WS rate-fire: the same over-budget attempt over WebSocket returns an
     error frame carrying code "rate_limited" (stable code propagation, not
     the generic "handler").

Server env this smoke expects:
  AERO_WS_RATE_STANDARD_PER_MIN=3   (tiny standard-tier workspace budget)
  AERO_RATE_LIMIT_PER_SEC high      (per-client limiter must not preempt)
  AERO_AUTH_RATE_LIMIT_PER_SEC high

Deterministic charge ledger (all in one minute window, budget = 3):
  send m1 (A)      → charge 1
  B × 5 doomed recalls → charge 0 (403 Forbidden each)
  A recalls m1     → charge 2 → 200 (conservation PROVEN)
  send m2 (A)      → charge 3
  A recalls m2     → charge 4 → 429 + Retry-After (REST rate-fire)
  A recalls m2 (WS) → error frame code "rate_limited" (WS rate-fire)
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030").rstrip("/")
if HOST.startswith("https://"):
    WS_HOST = f"wss://{HOST.removeprefix('https://')}"
elif HOST.startswith("http://"):
    WS_HOST = f"ws://{HOST.removeprefix('http://')}"
else:
    raise ValueError("AERO_HOST must start with http:// or https://")

FAILS: list[str] = []


def say(m):
    print(f"\033[1;36m▶ {m}\033[0m")


def ok(m):
    print(f"  \033[1;32m✓ {m}\033[0m")


def fail(m):
    print(f"  \033[1;31m✗ {m}\033[0m")
    FAILS.append(m)


def req(method, path, body=None, token=None, want_headers=False):
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r, timeout=15) as resp:
            buf = resp.read()
            parsed = json.loads(buf) if buf else None
            extra = dict(resp.headers) if want_headers else {}
            return resp.status, parsed, extra
    except urllib.error.HTTPError as e:
        buf = e.read()
        try:
            parsed = json.loads(buf) if buf else None
        except Exception:
            parsed = {"_raw": buf.decode(errors="ignore")[:200]}
        extra = dict(e.headers) if want_headers else {}
        return e.code, parsed, extra


async def recv_until(ws, pred, timeout=6, max_frames=200):
    for _ in range(max_frames):
        try:
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        except asyncio.TimeoutError:
            return None
        if pred(f):
            return f
    return None


async def main():
    ts = int(time.time())
    say("setup: register owner + member; room with both")
    _, a, _ = req("POST", "/api/auth/register", {"email": f"rgA+{ts}@aero.dev", "password": "password_1234", "display_name": "RgA"})
    _, b, _ = req("POST", "/api/auth/register", {"email": f"rgB+{ts}@aero.dev", "password": "password_1234", "display_name": "RgB"})
    A, B = a["access_token"], b["access_token"]
    Bpid = b["participant"]["id"]
    _, room, _ = req("POST", "/api/rooms", {"kind": "group", "name": f"rg-{ts}"}, token=A)
    rid = room["id"]
    req("POST", f"/api/rooms/{rid}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={rid[:8]}")

    say("owner sends m1 (charge 1/3)")
    st, m1, _ = req("POST", f"/api/rooms/{rid}/messages",
                    {"blocks": [{"type": "text", "content": "rate-gate target"}]}, token=A)
    if st in (200, 201):
        ok(f"m1 sent (status {st})")
    else:
        fail(f"m1 send failed: {st} {m1}")

    say("plain member's doomed recalls are Forbidden and consume no budget")
    for i in range(5):
        st, body, _ = req("POST", f"/api/messages/{m1['id']}/recall", token=B)
        if st != 403:
            fail(f"doomed recall #{i}: expected 403 Forbidden, got {st} {body}")
    ok("5 doomed recalls → 403 Forbidden each (role gate inside preflight)")

    say("owner's own recall still succeeds (budget survived the member's attempts)")
    st, recalled, _ = req("POST", f"/api/messages/{m1['id']}/recall", token=A)
    if st == 200 and recalled.get("recalled_at"):
        ok("owner recall 200 — conservation PROVEN (B's attempts charged nothing)")
    else:
        fail(f"owner recall: expected 200 + recalled_at, got {st} {recalled}")

    say("owner sends m2 (charge 3/3)")
    st, m2, _ = req("POST", f"/api/rooms/{rid}/messages",
                    {"blocks": [{"type": "text", "content": "rate-fire target"}]}, token=A)
    if st in (200, 201):
        ok("m2 sent")
    else:
        fail(f"m2 send failed: {st} {m2}")

    say("REST rate-fire: owner's next recall is over the 3/min window")
    st, body, headers = req("POST", f"/api/messages/{m2['id']}/recall", token=A, want_headers=True)
    if st == 429:
        ok("REST recall → 429")
    else:
        fail(f"REST recall: expected 429, got {st} {body}")
    retry_after = headers.get("Retry-After") or headers.get("retry-after")
    if retry_after and retry_after.isdigit() and 1 <= int(retry_after) <= 60:
        ok(f"Retry-After={retry_after} (fixed-window hint)")
    else:
        fail(f"Retry-After missing/invalid: {retry_after!r}")

    say("WS rate-fire: same over-budget attempt over WebSocket")
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        welcome = json.loads(await wa.recv())
        if welcome.get("type") != "welcome":
            fail(f"WS welcome missing: {welcome}")
        await wa.send(json.dumps({"type": "recall_message", "id": m2["id"]}))
        err = await recv_until(wa, lambda f: f.get("type") == "error")
        if err and err.get("code") == "rate_limited":
            ok(f"WS error frame code=rate_limited (msg={err.get('msg')!r})")
        else:
            fail(f"WS recall: expected error frame code=rate_limited, got {err}")

    if FAILS:
        say(f"SMOKE FAILED: {len(FAILS)} failure(s)")
        for f in FAILS:
            print(f"  ✗ {f}")
        sys.exit(1)
    say("smoke_recall_rate_gate passed")


if __name__ == "__main__":
    asyncio.run(main())
