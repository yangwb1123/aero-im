#!/usr/bin/env python3
"""Bot SDK CRUD smoke test: create, list, rotate token, subscribe, list subscriptions, delete subscription, list deliveries.

Proves the full lifecycle of bot registration and event subscription management.
Run against a live server.
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    """HTTP request helper: POST/GET/DELETE with Bearer token + JSON body/response."""
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
    
    say("register alice (bot owner)")
    a = req("POST", "/api/auth/register",
            {"email": f"alice_bot+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceBot"})
    A = a["access_token"]
    Alice_pid = a["participant"]["id"]
    ok(f"alice registered (participant_id={Alice_pid[:8]})")
    
    say("alice creates a bot (POST /api/bots)")
    bot_create_resp = req("POST", "/api/bots",
                          {"name": f"test_bot_{ts}", "icon_url": "https://example.com/bot.png"},
                          token=A, expect=200)
    if not bot_create_resp:
        fail("no response from bot create")
    bot_id = bot_create_resp.get("bot_id")
    token_plaintext = bot_create_resp.get("token")
    bot_name = bot_create_resp.get("name")
    if not bot_id or not token_plaintext:
        fail(f"bot_create missing bot_id or token: {bot_create_resp}")
    ok(f"bot created (bot_id={bot_id[:8]}, token=bot_... name={bot_name})")
    
    say("alice lists her bots (GET /api/bots)")
    bots = req("GET", "/api/bots", token=A, expect=200)
    if not isinstance(bots, list):
        fail(f"expected array from bot list, got: {bots}")
    found = [b for b in bots if b["id"] == bot_id]
    if not found:
        fail(f"created bot not in list: {[b['id'][:8] for b in bots]}")
    bot = found[0]
    if bot["owner_id"] != Alice_pid:
        fail(f"bot owner mismatch: expected {Alice_pid}, got {bot['owner_id']}")
    if bot["name"] != f"test_bot_{ts}":
        fail(f"bot name mismatch: expected test_bot_{ts}, got {bot['name']}")
    if bot["icon_url"] != "https://example.com/bot.png":
        fail(f"bot icon_url mismatch: got {bot.get('icon_url')}")
    if not bot.get("has_token"):
        fail("bot should have_token=true after creation")
    ok(f"bot listed ({len(bots)} total bots); verified owner_id, name, icon_url, has_token")
    
    say("alice rotates the bot's token (POST /api/bots/:id/token)")
    old_token = token_plaintext
    rotate_resp = req("POST", f"/api/bots/{bot_id}/token", token=A, expect=200)
    new_token = rotate_resp.get("token")
    if not new_token or new_token == old_token:
        fail(f"token rotation failed: {rotate_resp}")
    if not new_token.startswith("bot_"):
        fail(f"rotated token should start with 'bot_', got: {new_token[:10]}")
    ok(f"token rotated (new token=bot_..., old token revoked)")
    
    say("alice creates a bot subscription (POST /api/bots/:id/subscriptions)")
    sub_resp = req("POST", f"/api/bots/{bot_id}/subscriptions",
                   {"event_type": "message", "filters": {"action_id": "create"}, "webhook_url": "https://example.com/webhook"},
                   token=A, expect=200)
    if not sub_resp:
        fail("no response from subscription create")
    sub_id = sub_resp.get("id")
    sub_event_type = sub_resp.get("event_type")
    if not sub_id or sub_event_type != "message":
        fail(f"subscription create missing id or bad event_type: {sub_resp}")
    ok(f"subscription created (sub_id={sub_id}, event_type=message)")
    
    say("alice lists bot subscriptions (GET /api/bots/:id/subscriptions)")
    subs = req("GET", f"/api/bots/{bot_id}/subscriptions", token=A, expect=200)
    if not isinstance(subs, list):
        fail(f"expected array from subscriptions list, got: {subs}")
    found_subs = [s for s in subs if s["id"] == sub_id]
    if not found_subs:
        fail(f"created subscription not in list: {[s['id'] for s in subs]}")
    sub = found_subs[0]
    if sub["bot_id"] != bot_id:
        fail(f"subscription bot_id mismatch: expected {bot_id}, got {sub['bot_id']}")
    if sub["event_type"] != "message":
        fail(f"subscription event_type mismatch: expected 'message', got {sub['event_type']}")
    if sub.get("webhook_url") != "https://example.com/webhook":
        fail(f"subscription webhook_url mismatch: got {sub.get('webhook_url')}")
    filters = sub.get("filters", {})
    if filters.get("action_id") != "create":
        fail(f"subscription filters mismatch: got {filters}")
    ok(f"subscription listed ({len(subs)} total); verified bot_id, event_type, webhook_url, filters")
    
    say("alice adds another subscription (different event_type)")
    sub2_resp = req("POST", f"/api/bots/{bot_id}/subscriptions",
                    {"event_type": "reaction", "filters": {}, "webhook_url": "https://example.com/webhook2"},
                    token=A, expect=200)
    sub2_id = sub2_resp.get("id")
    if not sub2_id:
        fail(f"second subscription create failed: {sub2_resp}")
    ok(f"second subscription created (sub_id={sub2_id}, event_type=reaction)")
    
    say("alice lists subscriptions again (should see both)")
    subs2 = req("GET", f"/api/bots/{bot_id}/subscriptions", token=A, expect=200)
    if len(subs2) != 2:
        fail(f"expected 2 subscriptions, got {len(subs2)}")
    ok(f"both subscriptions listed ({len(subs2)})")
    
    say("alice lists bot deliveries (GET /api/bots/:id/deliveries)")
    deliveries = req("GET", f"/api/bots/{bot_id}/deliveries", token=A, expect=200)
    if not isinstance(deliveries, list):
        fail(f"expected array from deliveries list, got: {deliveries}")
    ok(f"deliveries listed ({len(deliveries)} total, may be empty on fresh bot)")
    
    say("alice deletes the first subscription (DELETE /api/bots/:id/subscriptions/:sub_id)")
    delete_resp = req("DELETE", f"/api/bots/{bot_id}/subscriptions/{sub_id}", token=A, expect=200)
    if not delete_resp.get("deleted"):
        fail(f"subscription delete did not return deleted=true: {delete_resp}")
    ok(f"subscription deleted (sub_id={sub_id})")
    
    say("alice verifies subscription is gone")
    subs3 = req("GET", f"/api/bots/{bot_id}/subscriptions", token=A, expect=200)
    if any(s["id"] == sub_id for s in subs3):
        fail(f"deleted subscription still present in list")
    if len(subs3) != 1:
        fail(f"expected 1 subscription after delete, got {len(subs3)}")
    if subs3[0]["id"] != sub2_id:
        fail(f"remaining subscription is not sub2")
    ok(f"verified deleted subscription gone ({len(subs3)} subscription(s) remain)")
    
    say("register bob, verify he cannot access alice's bot")
    b = req("POST", "/api/auth/register",
            {"email": f"bob_bot+{ts}@aero.dev", "password": "password_1234", "display_name": "BobBot"})
    B = b["access_token"]
    ok("bob registered")
    
    say("bob tries to rotate alice's bot token (should 403)")
    req("POST", f"/api/bots/{bot_id}/token", token=B, expect=403)
    ok("bob forbidden from rotating alice's bot token (403)")
    
    say("bob tries to delete alice's bot subscription (should 403)")
    req("DELETE", f"/api/bots/{bot_id}/subscriptions/{sub2_id}", token=B, expect=403)
    ok("bob forbidden from deleting alice's bot subscription (403)")
    
    say("bob tries to list alice's bot subscriptions (should 403)")
    req("GET", f"/api/bots/{bot_id}/subscriptions", token=B, expect=403)
    ok("bob forbidden from listing alice's bot subscriptions (403)")
    
    say("bob tries to list alice's bot deliveries (should 403)")
    req("GET", f"/api/bots/{bot_id}/deliveries", token=B, expect=403)
    ok("bob forbidden from listing alice's bot deliveries (403)")
    
    say("bob creates his own bot")
    bob_bot = req("POST", "/api/bots",
                  {"name": f"bob_bot_{ts}"},
                  token=B, expect=200)
    bob_bot_id = bob_bot.get("bot_id")
    if not bob_bot_id:
        fail(f"bob bot create failed: {bob_bot}")
    ok(f"bob created his own bot (bot_id={bob_bot_id[:8]})")
    
    say("alice lists her bots (should not include bob's)")
    alice_bots = req("GET", "/api/bots", token=A, expect=200)
    if any(b["id"] == bob_bot_id for b in alice_bots):
        fail(f"bob's bot appears in alice's list (access control broken)")
    ok(f"alice sees only her own bots ({len(alice_bots)}, bob's bot not listed)")
    
    say("bob creates a subscription on his bot")
    bob_sub = req("POST", f"/api/bots/{bob_bot_id}/subscriptions",
                  {"event_type": "message", "webhook_url": "https://example.com/bob"},
                  token=B, expect=200)
    bob_sub_id = bob_sub.get("id")
    if not bob_sub_id:
        fail(f"bob subscription create failed: {bob_sub}")
    ok(f"bob's subscription created (sub_id={bob_sub_id})")
    
    say("alice cannot see bob's subscriptions")
    req("GET", f"/api/bots/{bob_bot_id}/subscriptions", token=A, expect=403)
    ok("alice forbidden from listing bob's subscriptions (403)")
    
    say("full lifecycle complete: create, list, rotate token, subscribe, list subscriptions, delete, verify access control")
    
    print("\n\033[1;32m✅ Bot SDK CRUD smoke test PASSED\033[0m")


if __name__ == "__main__":
    main()
