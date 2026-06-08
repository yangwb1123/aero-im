#!/usr/bin/env python3
"""Wave-16 smoke: channel canvas, channel bookmarks (header links), stream
categories & discovery, creator subscriptions/tiers, workspace analytics,
people directory.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


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
            {"email": f"{tag}_w16+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W16"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob (member); workspace + channel")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave16 {ts}", "slug": f"w16-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"},
        token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"canvas-room-{ts}", "workspace_id": W}, token=A)["id"]
    # a couple of messages so analytics has something to count
    for t in ("hello world", "second message"):
        req("POST", f"/api/rooms/{R}/command", {"text": f"/me {t}"}, token=A)
    ok(f"workspace {W[:8]} + channel {R[:8]} (alice owner, bob ws-member, not room-member)")

    # ---------------- Channel canvas ----------------
    say("canvas: create / list / get / update / room-access gate / delete")
    cv = req("POST", f"/api/rooms/{R}/canvases",
             {"title": "Team Plan", "blocks": [{"type": "text", "text": "v1"}]}, token=A)
    cid = cv["id"]
    listed = as_list(req("GET", f"/api/rooms/{R}/canvases", token=A), "canvases")
    if not any(c.get("id") == cid for c in listed):
        fail(f"new canvas not in list: {listed}")
    got = req("GET", f"/api/canvases/{cid}", token=A)
    if got.get("title") != "Team Plan":
        fail(f"canvas title mismatch: {got}")
    req("PUT", f"/api/canvases/{cid}", {"title": "Team Plan v2"}, token=A, expect=[200])
    got2 = req("GET", f"/api/canvases/{cid}", token=A)
    if got2.get("title") != "Team Plan v2":
        fail(f"canvas update not applied: {got2}")
    # bob is not a room member → no access
    req("GET", f"/api/canvases/{cid}", token=B, expect=[403, 404])
    req("DELETE", f"/api/canvases/{cid}", token=A, expect=[200])
    req("GET", f"/api/canvases/{cid}", token=A, expect=[404])
    ok("canvas create/list/get/update ok; non-member 403/404; delete→404")

    # ---------------- Channel bookmarks ----------------
    say("channel bookmarks: add / list / patch / delete (header links)")
    bm = req("POST", f"/api/rooms/{R}/bookmarks",
             {"title": "Runbook", "url": "https://example.com/runbook", "emoji": "📘"}, token=A)
    bid = bm["id"]
    blist = as_list(req("GET", f"/api/rooms/{R}/bookmarks", token=A), "bookmarks")
    if not any(b.get("id") == bid for b in blist):
        fail(f"bookmark not in list: {blist}")
    req("PATCH", f"/api/channel-bookmarks/{bid}", {"title": "Runbook v2"}, token=A, expect=[200])
    blist2 = as_list(req("GET", f"/api/rooms/{R}/bookmarks", token=A), "bookmarks")
    if not any(b.get("id") == bid and b.get("title") == "Runbook v2" for b in blist2):
        fail(f"bookmark patch not applied: {blist2}")
    req("DELETE", f"/api/channel-bookmarks/{bid}", token=A, expect=[200])
    ok("channel bookmark add/list/patch/delete ok")

    # ---------------- Stream categories & discovery ----------------
    say("stream discovery: list categories, assign (owner-only), tags, browse")
    cats = as_list(req("GET", "/api/live/categories", token=A), "categories")
    slugs = {c.get("slug") for c in cats}
    if len(cats) < 5 or "gaming" not in slugs:
        fail(f"seeded categories missing: {cats}")
    S = req("POST", "/api/streams", {"title": "w16 stream", "protocol": "rtmp", "room_id": R}, token=A)["stream"]["id"]
    req("POST", f"/api/streams/{S}/category", {"slug": "gaming"}, token=A, expect=[200])
    # non-owner cannot file someone else's stream
    req("POST", f"/api/streams/{S}/category", {"slug": "music"}, token=B, expect=[403])
    # browse by category — endpoint works (list; empty until the stream goes live)
    brz = req("GET", "/api/live/categories/gaming/streams", token=A, expect=[200])
    if not isinstance(as_list(brz, "streams"), list):
        fail(f"category browse not a list: {brz}")
    req("POST", f"/api/streams/{S}/tags", {"tag": "speedrun"}, token=A, expect=[200])
    tags = as_list(req("GET", f"/api/streams/{S}/tags", token=A), "tags")
    if "speedrun" not in tags:
        fail(f"tag not stored: {tags}")
    req("DELETE", f"/api/streams/{S}/tags/speedrun", token=A, expect=[200])
    ok(f"categories seeded ({len(cats)}); assign owner-gated; tags round-trip; browse ok")

    # ---------------- Creator subscriptions / tiers ----------------
    say("creator subs: define tier, subscribe, list, self-sub 400, subscribers gate, unsub")
    tier = req("POST", f"/api/creators/{Apid}/tiers",
               {"name": "Gold", "price_cents": 500, "perks": "badge + emotes"}, token=A)
    tid = tier["id"]
    tiers = as_list(req("GET", f"/api/creators/{Apid}/tiers", token=A), "tiers")
    if not any(t.get("id") == tid for t in tiers):
        fail(f"tier not listed: {tiers}")
    req("POST", f"/api/creators/{Apid}/subscribe", {"tier_id": tid}, token=B, expect=[200])
    mysubs = as_list(req("GET", "/api/me/subscriptions", token=B), "subscriptions")
    if not any(s.get("creator_id") == Apid for s in mysubs):
        fail(f"bob's subscription missing: {mysubs}")
    subs = as_list(req("GET", f"/api/creators/{Apid}/subscribers", token=A), "subscribers")
    if not any(s.get("subscriber_id") == Bpid for s in subs):
        fail(f"alice's subscriber list missing bob: {subs}")
    # self-subscribe rejected; non-creator cannot view subscribers
    req("POST", f"/api/creators/{Apid}/subscribe", {"tier_id": tid}, token=A, expect=[400])
    req("GET", f"/api/creators/{Apid}/subscribers", token=B, expect=[403])
    req("DELETE", f"/api/creators/{Apid}/subscribe", token=B, expect=[200])
    ok("tier define + subscribe + list both sides; self-sub 400; subscribers 403; unsub ok")

    # ---------------- Workspace analytics ----------------
    say("analytics: admin overview/channels/timeline; member 403")
    ov = req("GET", f"/api/workspaces/{W}/analytics", token=A)
    for k in ("total_messages", "messages_last_7d", "total_rooms", "total_members", "active_members_7d"):
        if k not in ov:
            fail(f"analytics overview missing {k}: {ov}")
    if ov["total_messages"] < 2 or ov["total_rooms"] < 1 or ov["total_members"] < 2:
        fail(f"analytics counts look wrong: {ov}")
    req("GET", f"/api/workspaces/{W}/analytics/channels", token=A, expect=[200])
    req("GET", f"/api/workspaces/{W}/analytics/timeline", token=A, expect=[200])
    req("GET", f"/api/workspaces/{W}/analytics", token=B, expect=[403])
    ok(f"overview ok (msgs={ov['total_messages']}, rooms={ov['total_rooms']}, members={ov['total_members']}); member 403")

    # ---------------- People directory ----------------
    say("directory: list members; name filter narrows")
    dirall = as_list(req("GET", f"/api/workspaces/{W}/directory", token=A), "entries")
    pids = {e.get("participant_id") for e in dirall}
    if Apid not in pids or Bpid not in pids:
        fail(f"directory missing members: {dirall}")
    narrow = as_list(req("GET", f"/api/workspaces/{W}/directory?q=AliceW16", token=A), "entries")
    npids = {e.get("participant_id") for e in narrow}
    if Apid not in npids or Bpid in npids:
        fail(f"directory name filter wrong: {narrow}")
    # non-member cannot read another workspace's directory
    C, Cpid = register("carol", ts)
    req("GET", f"/api/workspaces/{W}/directory", token=C, expect=[403])
    ok(f"directory lists {len(dirall)} members; q= narrows to alice; non-member 403")

    print("\n\033[1;32m✅ Wave-16 smoke PASSED "
          "(canvas, channel bookmarks, stream discovery, creator subs, analytics, directory)\033[0m")


if __name__ == "__main__":
    main()
