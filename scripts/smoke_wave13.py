#!/usr/bin/env python3
"""Wave-13 smoke: group DM, AI action-items, channel join-requests, per-conversation export.

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
            {"email": f"{tag}_w13+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W13"})
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
    return (v.get("id") or (v.get("room") or {}).get("id")) if isinstance(v, dict) else None


async def ws_send(token, room, blocks):
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({"type": "send_message", "room_id": room, "blocks": blocks, "reply_to": None}))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message":
                return f["message"]
        return None


async def main():
    ts = int(time.time())
    say("setup: register alice/bob/carol + workspace")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    C, Cpid = register("carol", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave13 {ts}", "slug": f"w13-{ts}"}, token=A)["id"]
    for pid in (Bpid, Cpid):
        req("POST", f"/api/workspaces/{W}/members", {"participant_id": pid, "role": "member"}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} with alice/bob/carol")

    # ---------------- Group DM ----------------
    say("group DM: find-or-create among 3; idempotent; size guards")
    g1 = req("POST", "/api/group-dm", {"participant_ids": [Bpid, Cpid]}, token=A)
    G = room_id(g1)
    if not G:
        fail(f"no group dm: {g1}")
    g2 = req("POST", "/api/group-dm", {"participant_ids": [Cpid, Bpid]}, token=A)  # reordered
    if room_id(g2) != G:
        fail(f"group dm not idempotent/order-independent: {room_id(g2)} != {G}")
    ok(f"group DM {G[:8]} created + reused (order-independent)")
    req("POST", "/api/group-dm", {"participant_ids": [Bpid]}, token=A, expect=[400])  # 2 total -> use /dm
    ok("2-person group rejected (400, use /api/dm)")
    mine = as_list(req("GET", "/api/group-dm", token=A), "rooms")
    if not any(room_id({"id": x.get("id")}) == G or (isinstance(x, dict) and x.get("id") == G) for x in mine):
        fail(f"group dm not in list: {mine}")
    ok(f"group DM listed ({len(mine)})")

    # ---------------- AI action items ----------------
    say("ai action-items: extract tasks from a channel")
    Rc = req("POST", "/api/rooms", {"kind": "group", "name": f"ai-{ts}", "workspace_id": W}, token=A)["id"]
    await ws_send(A, Rc, [{"type": "text", "content": "TODO: alice to ship the release by Friday"}])
    await ws_send(A, Rc, [{"type": "text", "content": "bob will review the migration plan"}])
    await asyncio.sleep(0.3)
    ai = req("POST", f"/api/rooms/{Rc}/action-items", {}, token=A, expect=[200, 502])
    if ai is None:
        ok("action-items endpoint reachable (502 — no LLM configured)")
    else:
        if "action_items" not in ai:
            fail(f"missing action_items: {ai}")
        ok(f"action-items returned (len={len(str(ai.get('action_items')))})")

    # ---------------- Join requests ----------------
    say("join requests: request → owner approves → member; perms")
    Rj = req("POST", "/api/rooms", {"kind": "channel", "name": f"private-{ts}", "workspace_id": W}, token=A)["id"]
    # carol (not a member) requests to join
    jr = req("POST", f"/api/rooms/{Rj}/join-request", token=C)
    Jid = jr.get("id") or (jr.get("request") or {}).get("id")
    if not Jid:
        fail(f"no join-request id: {jr}")
    # bob (non-owner, non-admin) cannot list/approve
    req("GET", f"/api/rooms/{Rj}/join-requests", token=B, expect=[403])
    pend = as_list(req("GET", f"/api/rooms/{Rj}/join-requests", token=A), "requests")
    if not any(x.get("id") == Jid for x in pend):
        fail(f"request not pending: {pend}")
    ok(f"carol requested; owner sees it ({len(pend)} pending); non-owner blocked (403)")
    req("POST", f"/api/join-requests/{Jid}/approve", token=A, expect=[200, 204])
    members = as_list(req("GET", f"/api/rooms/{Rj}/members/list", token=A), "members")
    mids = [m if isinstance(m, str) else m.get("id") for m in members]
    if Cpid not in mids:
        fail(f"approved requester not a member: {mids}")
    ok("owner approved → carol is now a member")
    # already-member re-request → 409
    req("POST", f"/api/rooms/{Rj}/join-request", token=C, expect=[409])
    ok("already-member re-request rejected (409)")

    # ---------------- Conversation export ----------------
    say("conversation export: download a room's message history")
    Re = req("POST", "/api/rooms", {"kind": "group", "name": f"exp-{ts}", "workspace_id": W}, token=A)["id"]
    for i in range(3):
        await ws_send(A, Re, [{"type": "text", "content": f"export line {i}"}])
    await asyncio.sleep(0.3)
    exp = req("GET", f"/api/rooms/{Re}/export", token=A)
    msgs = as_list(exp, "messages")
    if len(msgs) < 3:
        fail(f"export missing messages: count={len(msgs)} body={str(exp)[:200]}")
    ok(f"exported {len(msgs)} messages (count={exp.get('count') if isinstance(exp, dict) else '?'})")
    req("GET", f"/api/rooms/{Re}/export", token=register('dan', ts)[0], expect=[403, 404])
    ok("export is room-access gated (non-member blocked)")

    print("\n\033[1;32m✅ Wave-13 smoke PASSED (group DM, AI action-items, join-requests, conversation export)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
