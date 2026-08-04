#!/usr/bin/env python3
"""Wave-17 smoke: out-of-office + auto-responder bot, org chart / manager
hierarchy, legal hold / retention exemption, tasks / to-do tracker, workspace
file browser, approvals workflow.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def warn(m): print(f"  \033[1;33m! {m}\033[0m")
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
            {"email": f"{tag}_w17+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W17"})
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
    say("setup: register alice (owner) + bob + carol; workspace + channel")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    C, Cpid = register("carol", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave17 {ts}", "slug": f"w17-{ts}"}, token=A)["id"]
    for pid in (Bpid, Cpid):
        req("POST", f"/api/workspaces/{W}/members", {"participant_id": pid, "role": "member"},
            token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"w17-room-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} + channel {R[:8]} (alice owner; bob room-member; carol workspace-only)")

    # ---------------- Out-of-office + auto-responder bot ----------------
    say("out-of-office: set status, then a DM to bob auto-replies once (out-of-band bot)")
    OOO_MSG = f"OOO until Monday {ts} - back soon"
    req("PUT", "/api/me/ooo", {"message": OOO_MSG}, token=B, expect=[200])
    vis = req("GET", f"/api/participants/{Bpid}/ooo", token=A)
    if not vis or vis.get("message") != OOO_MSG:
        fail(f"bob's OOO not visible: {vis}")
    # alice opens a 1:1 DM with bob and pings while bob is away
    dm = req("POST", f"/api/dm/{Bpid}", token=A)
    dmroom = dm["id"]
    req("POST", f"/api/rooms/{dmroom}/command", {"text": "/me pings while away"}, token=A)
    # poll the DM for bob's out-of-band auto-reply
    got_reply = False
    for _ in range(16):
        time.sleep(0.5)
        msgs = req("GET", f"/api/rooms/{dmroom}/messages", token=A)
        if OOO_MSG in json.dumps(as_list(msgs, "messages"), ensure_ascii=False):
            got_reply = True
            break
    if not got_reply:
        fail("ooo_bot did not deliver the auto-reply into the DM within timeout")
    ok("ooo_bot auto-replied bob's OOO message into the DM")
    req("DELETE", "/api/me/ooo", token=B, expect=[200])
    # after clearing, participants/:id/ooo shows nothing active
    cleared = req("GET", f"/api/participants/{Bpid}/ooo", token=A)
    if cleared not in (None, {}) and cleared.get("message"):
        fail(f"OOO not cleared: {cleared}")
    ok("OOO cleared (no longer active)")

    # ---------------- Org chart / manager hierarchy ----------------
    say("org chart: self-set manager, reports, reporting chain, self-as-manager 400, admin gate")
    org = f"/api/workspaces/{W}/participants"
    req("PUT", f"{org}/{Bpid}/manager", {"manager_id": Apid}, token=B, expect=[200])
    req("PUT", f"{org}/{Cpid}/manager", {"manager_id": Bpid}, token=C, expect=[200])
    mgr = req("GET", f"{org}/{Bpid}/manager", token=A)
    if mgr.get("manager_id") != Apid:
        fail(f"bob's manager wrong: {mgr}")
    reports = as_list(req("GET", f"{org}/{Apid}/reports", token=A), "reports")
    if Bpid not in reports:
        fail(f"alice's reports missing bob: {reports}")
    chain = as_list(req("GET", f"{org}/{Cpid}/chain", token=C), "chain")
    if chain[:2] != [Bpid, Apid]:
        fail(f"carol's chain wrong: {chain}")
    # self-as-manager rejected; non-self non-admin rejected
    req("PUT", f"{org}/{Apid}/manager", {"manager_id": Apid}, token=A, expect=[400])
    req("PUT", f"{org}/{Bpid}/manager", {"manager_id": Cpid}, token=C, expect=[403])
    req("DELETE", f"{org}/{Bpid}/manager", token=B, expect=[200])
    ok(f"manager/reports/chain ok (chain={[x[:6] for x in chain]}); self-mgr 400; non-self 403")

    # ---------------- Legal hold / retention exemption ----------------
    say("legal hold: admin places a room hold; member 403; release")
    hold = req("POST", f"/api/workspaces/{W}/legal-holds", {"room_id": R, "reason": "litigation hold"}, token=A)
    hid = hold["id"]
    holds = as_list(req("GET", f"/api/workspaces/{W}/legal-holds", token=A), "holds")
    if not any(h.get("id") == hid for h in holds):
        fail(f"hold not listed: {holds}")
    req("POST", f"/api/workspaces/{W}/legal-holds", {"room_id": R, "reason": "x"}, token=C, expect=[403])
    req("DELETE", f"/api/legal-holds/{hid}", token=A, expect=[200])
    holds2 = as_list(req("GET", f"/api/workspaces/{W}/legal-holds", token=A), "holds")
    if any(h.get("id") == hid for h in holds2):
        fail(f"released hold still active: {holds2}")
    ok("legal hold create/list/release ok; non-admin 403")

    # ---------------- Tasks / to-do tracker ----------------
    say("tasks: create+assign, list (room/mine), status, room-access gate, delete")
    tk = req("POST", f"/api/rooms/{R}/tasks", {"title": "ship the release", "assignee_id": Bpid}, token=A)
    tid = tk["id"]
    rtasks = as_list(req("GET", f"/api/rooms/{R}/tasks", token=A), "tasks")
    if not any(t.get("id") == tid for t in rtasks):
        fail(f"task not in room list: {rtasks}")
    mine = as_list(req("GET", "/api/me/tasks", token=B), "tasks")
    if not any(t.get("id") == tid for t in mine):
        fail(f"task not in bob's assigned list: {mine}")
    req("PATCH", f"/api/tasks/{tid}", {"status": "done"}, token=A, expect=[200])
    done = as_list(req("GET", f"/api/rooms/{R}/tasks?status=done", token=A), "tasks")
    if not any(t.get("id") == tid for t in done):
        fail(f"task not marked done: {done}")
    # carol is not a member of room R → no access to its tasks
    req("GET", f"/api/rooms/{R}/tasks", token=C, expect=[403, 404])
    req("DELETE", f"/api/tasks/{tid}", token=A, expect=[200])
    ok("task create/assign/list/status/delete ok; non-member 403")

    # ---------------- Workspace-wide file browser ----------------
    say("workspace files: member browse (route+gate); non-member 403")
    wf = req("GET", f"/api/workspaces/{W}/files", token=A, expect=[200])
    if not isinstance(as_list(wf, "files"), list):
        fail(f"workspace files not a list: {wf}")
    D, _Dpid = register("dave", ts)  # not a member of W
    req("GET", f"/api/workspaces/{W}/files", token=D, expect=[403])
    ok("workspace file browser reachable for member; non-member 403 "
       "(file-listing logic covered by the PG db_test)")

    # ---------------- Approvals workflow ----------------
    say("approvals: request → approver decides; non-approver 403; status filter")
    ap = req("POST", f"/api/workspaces/{W}/approvals",
             {"approver_id": Apid, "title": "PTO request", "details": "3 days"}, token=B)
    aid = ap["id"]
    incoming = as_list(req("GET", f"/api/workspaces/{W}/approvals/incoming", token=A), "approvals")
    if not any(a.get("id") == aid for a in incoming):
        fail(f"approval not in alice's incoming: {incoming}")
    outgoing = as_list(req("GET", f"/api/workspaces/{W}/approvals/outgoing", token=B), "approvals")
    if not any(a.get("id") == aid for a in outgoing):
        fail(f"approval not in bob's outgoing: {outgoing}")
    # requester (bob) is not the approver → cannot decide
    req("POST", f"/api/approvals/{aid}/approve", {"note": "self"}, token=B, expect=[403, 404])
    req("POST", f"/api/approvals/{aid}/approve", {"note": "approved"}, token=A, expect=[200])
    appr = as_list(req("GET", f"/api/workspaces/{W}/approvals/incoming?status=approved", token=A), "approvals")
    if not any(a.get("id") == aid and a.get("status") == "approved" for a in appr):
        fail(f"approval not marked approved: {appr}")
    ok("approval request/incoming/outgoing/decide ok; non-approver 403")

    print("\n\033[1;32m✅ Wave-17 smoke PASSED "
          "(out-of-office+bot, org chart, legal hold, tasks, workspace files, approvals)\033[0m")


if __name__ == "__main__":
    main()
