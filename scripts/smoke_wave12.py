#!/usr/bin/env python3
"""Wave-12 smoke: DM find-or-create, recurring messages, AI catch-up, reaction
detail (who-reacted), workspace default channels (auto-join on enroll).

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    ok_codes = {expect} if isinstance(expect, int) else (set(expect) if expect else None)
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if ok_codes is not None and resp.status not in ok_codes:
                fail(f"{method} {path}: want {sorted(ok_codes)} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if ok_codes is not None:
            if e.code not in ok_codes:
                fail(f"{method} {path}: want {sorted(ok_codes)} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def register(tag, ts):
    r = req("POST", "/api/auth/register",
            {"email": f"{tag}_w12+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W12"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def room_id(v):
    if isinstance(v, dict):
        return v.get("id") or (v.get("room") or {}).get("id")
    return None


async def ws_send(token, room, blocks, reply_to=None):
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({"type": "send_message", "room_id": room,
                                  "blocks": blocks, "reply_to": reply_to}))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message":
                return f["message"]
        return None


async def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob (member)")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave12 {ts}", "slug": f"w12-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} with alice+bob")

    # ---------------- DM find-or-create ----------------
    say("dm: open DM with bob, idempotent, self-DM 400")
    dm1 = req("POST", f"/api/dm/{Bpid}", token=A)
    R_dm = room_id(dm1)
    if not R_dm:
        fail(f"no dm room: {dm1}")
    dm2 = req("POST", f"/api/dm/{Bpid}", token=A)
    if room_id(dm2) != R_dm:
        fail(f"dm not idempotent: {room_id(dm2)} != {R_dm}")
    ok(f"dm room {R_dm[:8]} created + reused (idempotent)")
    req("POST", f"/api/dm/{Apid}", token=A, expect=[400])
    ok("self-DM rejected (400)")
    # bob sees the same DM from his side
    dm_b = req("POST", f"/api/dm/{Apid}", token=B)
    if room_id(dm_b) != R_dm:
        fail(f"bob's DM with alice differs: {room_id(dm_b)} != {R_dm}")
    ok("both participants resolve to the same DM room")

    # ---------------- Recurring messages ----------------
    say("recurring: create (daily), list, cancel; bad cadence 400")
    R = req("POST", "/api/rooms", {"kind": "group", "name": f"rec-{ts}", "workspace_id": W}, token=A)["id"]
    rec = req("POST", f"/api/rooms/{R}/recurring",
              {"blocks": [{"type": "text", "content": "daily standup reminder"}], "cadence": "daily"}, token=A)
    Rid = rec.get("id") or (rec.get("recurring") or {}).get("id")
    if not Rid:
        fail(f"no recurring id: {rec}")
    req("POST", f"/api/rooms/{R}/recurring",
        {"blocks": [{"type": "text", "content": "x"}], "cadence": "fortnightly"}, token=A, expect=[400])
    lst = as_list(req("GET", f"/api/rooms/{R}/recurring", token=A), "recurring", "messages")
    if not any((x.get("id") == Rid) for x in lst):
        fail(f"recurring not listed: {lst}")
    ok(f"recurring created + listed ({len(lst)}); bad cadence rejected (400)")
    req("DELETE", f"/api/recurring/{Rid}", token=B, expect=[403, 404])
    req("DELETE", f"/api/recurring/{Rid}", token=A, expect=[200, 204])
    ok("owner-scoped cancel (stranger blocked; owner cancels)")

    # ---------------- AI catch-up ----------------
    say("ai catch-up: summarize the caller's unread")
    Rc = req("POST", "/api/rooms", {"kind": "group", "name": f"catch-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{Rc}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    for i in range(3):
        await ws_send(A, Rc, [{"type": "text", "content": f"update number {i} about the launch"}])
    await asyncio.sleep(0.3)
    cu = req("POST", f"/api/rooms/{Rc}/catchup", token=B, expect=[200, 502])
    if cu is None:
        ok("catch-up endpoint reachable (AI service returned 502 — no LLM configured)")
    else:
        if "summary" not in cu:
            fail(f"catchup missing summary: {cu}")
        ok(f"catch-up summary returned (unread={cu.get('unread')}, len={len(str(cu.get('summary')))})")

    # ---------------- Reaction detail (who reacted) ----------------
    say("reaction detail: list participants behind each emoji")
    msg = await ws_send(A, Rc, [{"type": "text", "content": f"react to me {ts}"}])
    Mid = msg["id"]
    req("POST", f"/api/messages/{Mid}/reactions", {"emoji": "👍"}, token=A, expect=[200])
    req("POST", f"/api/messages/{Mid}/reactions", {"emoji": "👍"}, token=B, expect=[200])
    detail = req("GET", f"/api/messages/{Mid}/reactions/detail", token=A)
    reacts = as_list(detail, "reactions")
    thumbs = next((r for r in reacts if r.get("emoji") == "👍"), None)
    if not thumbs:
        fail(f"no 👍 in reaction detail: {detail}")
    parts = thumbs.get("participants", [])
    if Apid not in parts or Bpid not in parts:
        fail(f"reactors missing (want alice+bob): {thumbs}")
    ok(f"reaction detail lists both reactors ({len(parts)} on 👍)")

    # ---------------- Default channels (auto-join on enroll) ----------------
    say("default channels: mark default, new member auto-joins on enrollment")
    Rdef = req("POST", "/api/rooms", {"kind": "channel", "name": f"general-{ts}", "workspace_id": W}, token=A)["id"]
    req("PUT", f"/api/workspaces/{W}/default-channels/{Rdef}", token=A, expect=[200, 204])
    defs = as_list(req("GET", f"/api/workspaces/{W}/default-channels", token=A), "rooms", "channels")
    if Rdef not in defs:
        fail(f"default channel not listed: {defs}")
    req("PUT", f"/api/workspaces/{W}/default-channels/{Rdef}", token=B, expect=[403])
    ok(f"channel marked default ({len(defs)}); non-admin blocked (403)")
    # enroll a brand-new member -> should auto-join the default channel
    C, Cpid = register("carol", ts)
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Cpid, "role": "member"}, token=A, expect=[200, 204])
    await asyncio.sleep(0.3)
    members = as_list(req("GET", f"/api/rooms/{Rdef}/members/list", token=A), "members")
    member_ids = [m if isinstance(m, str) else m.get("id") for m in members]
    if Cpid not in member_ids:
        fail(f"new member not auto-joined to default channel: {member_ids}")
    ok("new workspace member auto-joined the default channel")

    print("\n\033[1;32m✅ Wave-12 smoke PASSED (dm, recurring, ai catch-up, reaction detail, default channels)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
