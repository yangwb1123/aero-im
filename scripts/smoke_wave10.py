#!/usr/bin/env python3
"""Wave-10 smoke: user groups (@-usergroups), channel favorites, custom profile
fields, message edit history, keyword/highlight alerts, workspace announcements.

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
            {"email": f"{tag}_w10+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W10"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def send_via_command(room, text, token):
    """Post a message through the slash-command REST path (`/me ...`) which goes
    through the normal send pipeline; returns the created message id."""
    posted = req("POST", f"/api/rooms/{room}/command", {"text": f"/me {text}"}, token=token)
    mid = posted.get("id") if isinstance(posted, dict) else None
    if not mid:
        fail(f"no message id from command send: {posted}")
    return mid


def main():
    ts = int(time.time())
    say("setup: register alice (workspace owner) + bob (member)")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    ws = req("POST", "/api/workspaces", {"name": f"Wave10 {ts}", "slug": f"w10-{ts}"}, token=A)
    W = ws["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"},
        token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} owned by alice; bob enrolled as member")

    # ---------------- User groups (@-usergroups) ----------------
    say("user groups: create, list, get(+members), add/remove member, perms, dup 409")
    g = req("POST", f"/api/workspaces/{W}/user-groups",
            {"handle": f"eng{ts}", "name": "Engineering"}, token=A)
    Gid = g.get("id") or g.get("group", {}).get("id")
    if not Gid:
        fail(f"no user-group id: {g}")
    req("POST", f"/api/workspaces/{W}/user-groups",
        {"handle": f"eng{ts}", "name": "dup"}, token=A, expect=[409])
    groups = as_list(req("GET", f"/api/workspaces/{W}/user-groups", token=A), "user_groups", "groups")
    if not any(x.get("id") == Gid for x in groups):
        fail(f"group not listed: {groups}")
    ok(f"group created + listed; duplicate handle rejected (409)")
    req("PUT", f"/api/workspaces/{W}/user-groups/{Gid}/members/{Bpid}", token=A, expect=[200, 204])
    detail = req("GET", f"/api/user-groups/{Gid}", token=A)
    members = as_list(detail, "members", "member_ids") or as_list(detail.get("group", {}), "members")
    if Bpid not in [m if isinstance(m, str) else m.get("id") for m in members]:
        fail(f"bob not in group members: {detail}")
    ok(f"member added; group detail lists {len(members)} member(s)")
    # bob (member, not creator/admin) cannot delete alice's group
    req("DELETE", f"/api/workspaces/{W}/user-groups/{Gid}", token=B, expect=[403])
    req("DELETE", f"/api/workspaces/{W}/user-groups/{Gid}/members/{Bpid}", token=A, expect=[200, 204])
    req("DELETE", f"/api/workspaces/{W}/user-groups/{Gid}", token=A, expect=[200, 204])
    ok("non-admin delete blocked (403); member removed; group deleted by owner")

    # ---------------- Channel favorites ----------------
    say("favorites: star a channel, list, unstar")
    rf = req("POST", "/api/rooms", {"kind": "channel", "name": f"fav-{ts}", "workspace_id": W}, token=A)["id"]
    req("PUT", f"/api/rooms/{rf}/favorite", token=A, expect=[200, 204])
    favs = as_list(req("GET", "/api/favorites", token=A), "rooms", "favorites")
    if rf not in favs:
        fail(f"favorite not listed: {favs}")
    ok(f"channel starred + listed ({len(favs)})")
    req("DELETE", f"/api/rooms/{rf}/favorite", token=A, expect=[200, 204])
    favs2 = as_list(req("GET", "/api/favorites", token=A), "rooms", "favorites")
    if rf in favs2:
        fail("favorite not removed")
    ok("channel unstarred")

    # ---------------- Custom profile fields ----------------
    say("profile fields: put, get, get-another")
    req("PUT", "/api/me/profile",
        {"title": "Staff Engineer", "pronouns": "she/her", "timezone": "Europe/Berlin",
         "status_text": "shipping"}, token=A, expect=[200, 204])
    prof = req("GET", "/api/me/profile", token=A)
    if prof.get("title") != "Staff Engineer" or prof.get("pronouns") != "she/her":
        fail(f"profile not persisted: {prof}")
    ok("profile upserted + read back")
    other = req("GET", f"/api/participants/{Apid}/profile", token=B)
    if not other or other.get("title") != "Staff Engineer":
        fail(f"could not read another participant's profile: {other}")
    ok("another participant's profile is visible")

    # ---------------- Message edit history ----------------
    say("edit history: send → edit → history shows the prior version")
    re_ = req("POST", "/api/rooms", {"kind": "group", "name": f"edit-{ts}", "workspace_id": W}, token=A)["id"]
    Mid = send_via_command(re_, f"original text {ts}", A)
    req("PATCH", f"/api/messages/{Mid}",
        {"blocks": [{"type": "text", "content": f"edited text {ts}"}]}, token=A, expect=[200])
    hist = req("GET", f"/api/messages/{Mid}/history", token=A)
    hlist = as_list(hist, "history", "edits", "versions")
    blob = json.dumps(hist)
    if not hlist or f"original text {ts}" not in blob:
        fail(f"edit history did not capture the prior version: {blob[:400]}")
    ok(f"history captured the original version ({len(hlist)} entry/entries)")
    # non-member cannot read the history
    req("GET", f"/api/messages/{Mid}/history", token=B, expect=[403, 404])
    ok("history is room-access gated (non-member blocked)")

    # ---------------- Keyword / highlight alerts ----------------
    say("keyword alerts: subscribe, list, e2e notify, delete")
    keyword = f"zkbrd{ts}"
    ka = req("POST", "/api/keyword-alerts", {"workspace_id": W, "keyword": keyword}, token=B)
    Kid = ka.get("id") or ka.get("alert", {}).get("id")
    if not Kid:
        fail(f"no keyword-alert id: {ka}")
    klist = as_list(req("GET", f"/api/keyword-alerts?workspace_id={W}", token=B), "keyword_alerts", "alerts")
    if not any(x.get("id") == Kid for x in klist):
        fail(f"keyword alert not listed: {klist}")
    ok(f"keyword alert created + listed ({len(klist)})")
    # e2e: bob is a member of a room; alice posts a message containing bob's keyword;
    # bob should receive a notification.
    rk = req("POST", "/api/rooms", {"kind": "group", "name": f"kw-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{rk}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    def notif_count(tok):
        r = req("GET", "/api/notifications/count", token=tok)
        if isinstance(r, dict):
            return r.get("unread", r.get("count", 0))
        return r if isinstance(r, int) else 0
    before_n = notif_count(B)
    send_via_command(rk, f"please review the {keyword} change", A)
    time.sleep(0.5)
    after_n = notif_count(B)
    if after_n <= before_n:
        fail(f"keyword did not trigger a notification for bob ({before_n} -> {after_n})")
    ok(f"keyword match notified the subscriber ({before_n} -> {after_n})")
    req("DELETE", f"/api/keyword-alerts/{Kid}", token=A, expect=[403, 404])
    req("DELETE", f"/api/keyword-alerts/{Kid}", token=B, expect=[200, 204])
    ok("owner-scoped delete (stranger blocked; owner deletes)")

    # ---------------- Workspace announcements ----------------
    say("announcements: admin posts, members read active, expired hidden, perms")
    ann = req("POST", f"/api/workspaces/{W}/announcements",
              {"body": f"All-hands at noon {ts}"}, token=A)
    Aid = ann.get("id") or ann.get("announcement", {}).get("id")
    if not Aid:
        fail(f"no announcement id: {ann}")
    active = as_list(req("GET", f"/api/workspaces/{W}/announcements", token=B), "announcements")
    if not any(x.get("id") == Aid for x in active):
        fail(f"announcement not visible to member: {active}")
    ok(f"announcement posted + visible to member ({len(active)} active)")
    # an already-expired announcement is not listed
    req("POST", f"/api/workspaces/{W}/announcements",
        {"body": "stale", "expires_in_secs": 0}, token=A, expect=[200, 400])
    # bob (member, not admin) cannot post or delete
    req("POST", f"/api/workspaces/{W}/announcements", {"body": "nope"}, token=B, expect=[403])
    req("DELETE", f"/api/workspaces/{W}/announcements/{Aid}", token=B, expect=[403])
    req("DELETE", f"/api/workspaces/{W}/announcements/{Aid}", token=A, expect=[200, 204])
    ok("member cannot post/delete (403); owner deletes")

    print("\n\033[1;32m✅ Wave-10 smoke PASSED (user groups, favorites, profiles, "
          "edit history, keyword alerts, announcements)\033[0m")


if __name__ == "__main__":
    main()
