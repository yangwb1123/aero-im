#!/usr/bin/env python3
"""ROADMAP3 Wave-C live smoke (方向一/三/四/五 application-visible surface).

Covers, against a foreground server:
  - 方向一: RoomEvent `seq` stamping (message + edited frames carry a growing
    top-level `seq`), multi-device Read fan-out, WS `?since=` backfill
    truncation frame (`type=backfill, truncated=true, next_since`) + REST
    continuation.
  - 方向四: async (off-hot-path) notification dispatch — @mention lands in the
    inbox shortly after send returns (poll, never assume synchronous).
  - 方向五: per-workspace rate tiers — owner PUT/GET /api/workspaces/:id/rate-tier,
    non-member 403, premium tier ceiling trips 429 on room reads, unlimited
    clears it; transactional message-delete audit (`message.deleted` in the
    workspace audit log).
  - 方向三: /api/ai/ask 200s through the new FTS-fusion rerank path (heuristic
    degrade without an LLM key); /metrics shows the moderation pipeline +
    ws-rate counters.

Server env this smoke expects:
  AERO_WS_RATE_STANDARD_PER_MIN high (>=100000), AERO_WS_RATE_PREMIUM_PER_MIN=5,
  AERO_RATE_LIMIT_PER_SEC high, AERO_AI_MODERATION=1.
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:8099")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")

FAILS: list[str] = []


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m):
    print(f"  \033[1;31m✗ {m}\033[0m")
    FAILS.append(m)


def req(method, path, body=None, token=None, expect=None):
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
            return resp.status, (json.loads(buf) if buf else None)
    except urllib.error.HTTPError as e:
        buf = e.read()
        try:
            return e.code, json.loads(buf) if buf else None
        except Exception:
            return e.code, {"_raw": buf.decode(errors="ignore")[:200]}


async def recv_until(ws, pred, timeout=6, max_frames=400):
    """Read frames until pred(frame) is truthy; return that frame (or None)."""
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
    say("setup: register alice + bob; shared room in the default workspace")
    _, a = req("POST", "/api/auth/register", {"email": f"w3cA+{ts}@aero.dev", "password": "password_1234", "display_name": "W3cA"})
    _, b = req("POST", "/api/auth/register", {"email": f"w3cB+{ts}@aero.dev", "password": "password_1234", "display_name": "W3cB"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    _, room = req("POST", "/api/rooms", {"kind": "group", "name": f"w3c-{ts}"}, token=A)
    r1 = room["id"]
    req("POST", f"/api/rooms/{r1}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={r1[:8]}")

    # ---- 方向一: seq on message + edited frames; multi-device read fan-out ----
    say("方向一: event seq stamping + multi-device read")
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa, \
               websockets.connect(f"{WS_HOST}/ws?token={B}") as wb, \
               websockets.connect(f"{WS_HOST}/ws?token={B}") as wb2:
        for w in (wa, wb, wb2):
            await w.send(json.dumps({"type": "join_room", "room_id": r1}))
        await asyncio.sleep(0.3)
        await wa.send(json.dumps({"type": "send_message", "room_id": r1,
                                  "blocks": [{"type": "text", "content": "hello seq"}]}))
        mf = await recv_until(wb, lambda f: f.get("type") == "message")
        if mf and isinstance(mf.get("seq"), int) and mf["seq"] >= 1:
            ok(f"message frame carries seq={mf['seq']}")
        else:
            fail(f"message frame missing integer seq: {mf}")
        mid = mf["message"]["id"] if mf else None

        await wa.send(json.dumps({"type": "edit_message", "id": mid,
                                  "blocks": [{"type": "text", "content": "hello seq v2"}]}))
        ef = await recv_until(wb, lambda f: f.get("type") == "edited")
        if ef and isinstance(ef.get("seq"), int) and mf and ef["seq"] > mf["seq"]:
            ok(f"edited frame seq {ef['seq']} > message seq {mf['seq']}")
        else:
            fail(f"edited frame seq not greater: {ef and ef.get('seq')} vs {mf and mf.get('seq')}")

        # device B1 marks read over REST; device B2 must see the Read event (self).
        st, _ = req("POST", f"/api/rooms/{r1}/read", {"last_message_id": mid}, token=B)
        rf = await recv_until(wb2, lambda f: f.get("type") == "read" and f.get("participant") == Bpid)
        if st == 200 and rf:
            ok("second device received own read event (multi-device convergence)")
        else:
            fail(f"read event not seen on second device (status={st}, frame={rf})")

    # ---- 方向四: async notification dispatch (@mention) ----
    say("方向四: off-hot-path notification dispatch")
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        await wa.send(json.dumps({"type": "join_room", "room_id": r1}))
        await asyncio.sleep(0.2)
        await wa.send(json.dumps({"type": "send_message", "room_id": r1,
                                  "blocks": [{"type": "text", "content": "ping"},
                                             {"type": "mention", "participant": Bpid}]}))
        await recv_until(wa, lambda f: f.get("type") == "message")
    got = False
    for _ in range(20):  # dispatch is now spawned — poll up to ~4s
        _, inbox = req("GET", "/api/notifications", token=B)
        rows = inbox if isinstance(inbox, list) else (inbox or {}).get("notifications", [])
        if any(n.get("kind") == "mention" for n in rows):
            got = True
            break
        await asyncio.sleep(0.2)
    ok("mention notification delivered asynchronously") if got else fail("mention notification never arrived")

    # ---- 方向一: backfill truncation + REST continuation ----
    say("方向一: ?since= backfill truncation signal + REST continuation (205 msgs)")
    first_id = None
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        await wa.send(json.dumps({"type": "join_room", "room_id": r1}))
        await asyncio.sleep(0.2)
        for i in range(205):
            await wa.send(json.dumps({"type": "send_message", "room_id": r1,
                                      "blocks": [{"type": "text", "content": f"burst {i}"}]}))
        # drain own echoes; capture the first burst message id
        seen = 0
        while seen < 205:
            f = await recv_until(wa, lambda f: f.get("type") == "message", timeout=10)
            if f is None:
                break
            if f["message"]["blocks"][0].get("content", "").startswith("burst"):
                if first_id is None:
                    first_id = f["message"]["id"]
                seen += 1
    if first_id is None:
        fail("burst send failed (no first id)")
    else:
        async with websockets.connect(f"{WS_HOST}/ws?token={B}&since={first_id}") as wb:
            replayed, trunc = 0, None
            while True:
                f = await recv_until(wb, lambda f: f.get("type") in ("message", "backfill"), timeout=6)
                if f is None:
                    break
                if f["type"] == "message":
                    replayed += 1
                else:
                    trunc = f
                    break
            if trunc and trunc.get("truncated") and trunc.get("next_since"):
                ok(f"backfill truncated after {replayed} replayed; next_since={str(trunc['next_since'])[:8]}…")
                st, rest = req("GET", f"/api/rooms/{r1}/messages?since={trunc['next_since']}", token=B)
                n = len(rest) if isinstance(rest, list) else len((rest or {}).get("messages", []))
                if st == 200 and n >= 1:
                    ok(f"REST continuation returned {n} remaining messages")
                else:
                    fail(f"REST continuation failed (status={st}, n={n})")
            else:
                fail(f"no truncation frame (replayed={replayed}, last={trunc})")

    # ---- 方向五: per-workspace rate tiers ----
    say("方向五: per-workspace rate tier (premium ceiling=5/min on this server)")
    _, ws = req("POST", "/api/workspaces", {"name": f"tier-{ts}", "slug": f"tier-{ts}"}, token=A)
    wsid = ws["id"]
    _, r2 = req("POST", "/api/rooms", {"kind": "group", "name": f"tier-room-{ts}", "workspace_id": wsid}, token=A)
    r2 = r2["id"]
    st, _ = req("PUT", f"/api/workspaces/{wsid}/rate-tier", {"tier": "premium"}, token=A)
    st2, tier = req("GET", f"/api/workspaces/{wsid}/rate-tier", token=A)
    if st in (200, 204) and st2 == 200 and (tier or {}).get("tier") == "premium":
        ok("owner set + read back tier=premium")
    else:
        fail(f"rate-tier set/get failed ({st}/{st2}/{tier})")
    st, _ = req("PUT", f"/api/workspaces/{wsid}/rate-tier", {"tier": "unlimited"}, token=B)
    ok("non-member tier change 403") if st == 403 else fail(f"non-member tier change gave {st}")

    codes = [req("GET", f"/api/rooms/{r2}/messages", token=A)[0] for _ in range(10)]
    if 429 in codes:
        ok(f"premium ceiling tripped 429 ({codes.count(429)}/10)")
    else:
        fail(f"no 429 under premium ceiling: {codes}")
    req("PUT", f"/api/workspaces/{wsid}/rate-tier", {"tier": "unlimited"}, token=A)
    codes = [req("GET", f"/api/rooms/{r2}/messages", token=A)[0] for _ in range(10)]
    if 429 not in codes:
        ok("unlimited tier clears the ceiling")
    else:
        fail(f"429 still present on unlimited: {codes}")

    # ---- 方向五: transactional message-delete audit ----
    say("方向五: transactional delete audit")
    async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        await wa.send(json.dumps({"type": "join_room", "room_id": r2}))
        await asyncio.sleep(0.2)
        await wa.send(json.dumps({"type": "send_message", "room_id": r2,
                                  "blocks": [{"type": "text", "content": "to be deleted"}]}))
        df = await recv_until(wa, lambda f: f.get("type") == "message")
    did = df["message"]["id"]
    st, _ = req("DELETE", f"/api/messages/{did}", token=A)
    _, audit = req("GET", f"/api/workspaces/{wsid}/audit", token=A)
    dump = json.dumps(audit, ensure_ascii=False)
    if st in (200, 204) and "message.deleted" in dump:
        ok("message.deleted audit row present (transactional path)")
    else:
        fail(f"delete audit missing (status={st}, audit={dump[:200]})")

    # ---- 方向三: rerank-backed ask + moderation/ws-rate metrics ----
    say("方向三: rerank-path ask + pipeline metrics")
    st, ans = req("POST", "/api/ai/ask", {"room_id": r1, "question": "what was the burst about?"}, token=A)
    ok("ai ask 200 through fused retrieval (degraded backend)") if st == 200 else fail(f"ai ask gave {st}: {ans}")
    st, _ = req("GET", "/api/me", token=A)
    metrics = urllib.request.urlopen(HOST + "/metrics", timeout=10).read().decode()
    ok("moderation pipeline metrics present") if "aero_ai_moderation" in metrics else fail("no aero_ai_moderation_* in /metrics")
    ok("ws-rate metrics present") if "aero_ws_rate" in metrics else fail("no aero_ws_rate_* in /metrics")

    print()
    if FAILS:
        print(f"\033[1;31mFAILED {len(FAILS)} check(s):\033[0m")
        for f in FAILS:
            print(f"  - {f}")
        sys.exit(1)
    print("\033[1;32mALL ROADMAP3 WAVE-C SMOKE CHECKS PASSED\033[0m")


asyncio.run(main())
