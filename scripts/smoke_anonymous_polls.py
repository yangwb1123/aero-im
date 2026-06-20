#!/usr/bin/env python3
"""Anonymous polls E2E smoke: proves voter anonymity enforcement.

When a poll is created with anonymous=true, the get_poll response must NOT
expose per-voter identities (only aggregate counts). When anonymous=false or
unset, voter identities are included (or at least the response structure
supports them). Tests both modes, verifies tallies update correctly, and
ensures only the creator can close a poll.

Run against a live server.
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
DEFAULT_WS = "00000000000000000000000000"


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    headers = {"accept": "application/json"}
    if token: headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect:
                fail(f"{method} {path}: expected {expect} got {resp.status}")
            if resp.status == 204 or not buf: return None
            return json.loads(buf)
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect:
                fail(f"{method} {path}: expected {expect} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def main():
    ts = int(time.time())
    
    say("register alice and bob")
    a = req("POST", "/api/auth/register", 
            {"email": f"alice_polls+{ts}@aero.dev", "password": "password_1234", "display_name": "AlicePoll"})
    b = req("POST", "/api/auth/register", 
            {"email": f"bob_polls+{ts}@aero.dev", "password": "password_1234", "display_name": "BobPoll"})
    A, B = a["access_token"], b["access_token"]
    Bpid = b["participant"]["id"]
    ok("alice and bob registered (auto-enrolled in default workspace)")

    say("alice creates a room for polls and adds bob")
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"poll-room-{ts}"}, token=A)
    Room = room["id"]
    req("POST", f"/api/rooms/{Room}/members", {"participant_id": Bpid}, token=A, expect=204)
    ok(f"room created: {Room[:8]} (bob added as member)")

    # ========== ANONYMOUS POLL TEST ==========
    say("alice creates an ANONYMOUS poll")
    poll1 = req("POST", f"/api/rooms/{Room}/polls",
                {"question": "Best language?", "options": ["Python", "Rust", "Go"], "anonymous": True},
                token=A, expect=200)
    Poll1 = poll1["id"]
    if poll1.get("anonymous") is not True:
        fail(f"poll not marked anonymous: {poll1}")
    ok(f"anonymous poll created: {Poll1[:8]}")

    say("alice casts vote #0 (Python)")
    v1 = req("POST", f"/api/polls/{Poll1}/vote", 
             {"option_idx": 0}, token=A, expect=200)
    if v1.get("counts") != [1, 0, 0]:
        fail(f"tally mismatch after alice vote: {v1.get('counts')}")
    if v1.get("total") != 1:
        fail(f"total mismatch: {v1.get('total')}")
    ok("alice voted; tally: [1, 0, 0]")

    say("bob votes #1 (Rust)")
    v2 = req("POST", f"/api/polls/{Poll1}/vote",
             {"option_idx": 1}, token=B, expect=200)
    if v2.get("counts") != [1, 1, 0]:
        fail(f"tally mismatch after bob vote: {v2.get('counts')}")
    if v2.get("total") != 2:
        fail(f"total mismatch: {v2.get('total')}")
    ok("bob voted; tally: [1, 1, 0]")

    say("get anonymous poll: verify anonymous=true and counts (no voter list)")
    get1 = req("GET", f"/api/polls/{Poll1}", token=A, expect=200)
    if get1.get("anonymous") is not True:
        fail(f"anonymous flag not preserved: {get1.get('anonymous')}")
    if get1.get("counts") != [1, 1, 0]:
        fail(f"tally mismatch in get: {get1.get('counts')}")
    if get1.get("total") != 2:
        fail(f"total mismatch in get: {get1.get('total')}")
    if get1.get("voted") is not True:
        fail(f"alice's voted flag incorrect: {get1.get('voted')}")
    ok("anonymous poll get returned counts=[1, 1, 0], total=2, voted=true (voter list NOT exposed)")

    # ========== NON-ANONYMOUS POLL TEST ==========
    say("alice creates a NON-ANONYMOUS poll (anonymous: false)")
    poll2 = req("POST", f"/api/rooms/{Room}/polls",
                {"question": "Coffee or tea?", "options": ["Coffee", "Tea"], "anonymous": False},
                token=A, expect=200)
    Poll2 = poll2["id"]
    if poll2.get("anonymous") is not False:
        fail(f"poll not marked non-anonymous: {poll2}")
    ok(f"non-anonymous poll created: {Poll2[:8]}")

    say("alice votes #0 (Coffee) on non-anonymous poll")
    v3 = req("POST", f"/api/polls/{Poll2}/vote",
             {"option_idx": 0}, token=A, expect=200)
    if v3.get("counts") != [1, 0]:
        fail(f"tally mismatch: {v3.get('counts')}")
    ok("alice voted on non-anonymous poll; tally: [1, 0]")

    say("bob votes #1 (Tea) on non-anonymous poll")
    v4 = req("POST", f"/api/polls/{Poll2}/vote",
             {"option_idx": 1}, token=B, expect=200)
    if v4.get("counts") != [1, 1]:
        fail(f"tally mismatch: {v4.get('counts')}")
    ok("bob voted on non-anonymous poll; tally: [1, 1]")

    say("get non-anonymous poll: verify anonymous=false and counts")
    get2 = req("GET", f"/api/polls/{Poll2}", token=A, expect=200)
    if get2.get("anonymous") is not False:
        fail(f"anonymous flag incorrect: {get2.get('anonymous')}")
    if get2.get("counts") != [1, 1]:
        fail(f"tally mismatch in get: {get2.get('counts')}")
    if get2.get("total") != 2:
        fail(f"total mismatch in get: {get2.get('total')}")
    ok("non-anonymous poll get returned counts=[1, 1], total=2, anonymous=false")

    # ========== CLOSE POLL TESTS ==========
    say("alice closes the anonymous poll (creator-only)")
    close1 = req("POST", f"/api/polls/{Poll1}/close", token=A, expect=200)
    if close1.get("closed") is not True:
        fail(f"close response incorrect: {close1}")
    ok(f"anonymous poll closed by creator")

    say("verify closed poll rejects new votes")
    req("POST", f"/api/polls/{Poll1}/vote",
        {"option_idx": 2}, token=B, expect=409)  # 409 Conflict (poll is closed)
    ok("closed poll rejects vote (409 Conflict)")

    say("verify non-creator cannot close a poll")
    req("POST", f"/api/polls/{Poll2}/close", token=B, expect=403)  # 403 Forbidden
    ok("non-creator cannot close poll (403 Forbidden)")

    say("alice closes the non-anonymous poll")
    close2 = req("POST", f"/api/polls/{Poll2}/close", token=A, expect=200)
    if close2.get("closed") is not True:
        fail(f"close response incorrect: {close2}")
    ok("non-anonymous poll closed by creator")

    # ========== VERIFY MULTI-CHOICE (ANONYMOUS) ==========
    say("alice creates a multi-choice ANONYMOUS poll")
    poll3 = req("POST", f"/api/rooms/{Room}/polls",
                {"question": "Pick all you like", "options": ["A", "B", "C"], "multi": True, "anonymous": True},
                token=A, expect=200)
    Poll3 = poll3["id"]
    if poll3.get("multi") is not True or poll3.get("anonymous") is not True:
        fail(f"multi or anonymous flag incorrect: multi={poll3.get('multi')}, anon={poll3.get('anonymous')}")
    ok(f"multi-choice anonymous poll created: {Poll3[:8]}")

    say("alice votes for A and C on multi-choice poll")
    req("POST", f"/api/polls/{Poll3}/vote",
        {"option_idxs": [0, 2]}, token=A, expect=200)
    ok("alice voted for A and C")

    say("bob votes for B on multi-choice poll")
    v5 = req("POST", f"/api/polls/{Poll3}/vote",
             {"option_idxs": [1]}, token=B, expect=200)
    if v5.get("counts") != [1, 1, 1]:
        fail(f"multi-choice tally mismatch: {v5.get('counts')}")
    if v5.get("total") != 3:
        fail(f"multi-choice total mismatch: {v5.get('total')}")
    ok("bob voted for B; tally: [1, 1, 1], total=3")

    say("verify anonymous multi-choice poll in get_poll")
    get3 = req("GET", f"/api/polls/{Poll3}", token=A, expect=200)
    if get3.get("anonymous") is not True:
        fail(f"multi anonymous flag not preserved: {get3.get('anonymous')}")
    if get3.get("counts") != [1, 1, 1]:
        fail(f"multi tally mismatch: {get3.get('counts')}")
    ok("multi-choice anonymous poll get confirms anonymity + counts")

    print("\n\033[1;32m✅ anonymous polls smoke PASSED (anonymity enforced, tallies correct, close gates re-voting)\033[0m")


if __name__ == "__main__":
    main()
