# Aero IM — Web Debug Client

A single-page, zero-dependency, native JavaScript client for talking to the
Aero IM Rust backend during integration work.

It speaks the HTTP `/api/*` surface for auth, rooms, and message history and
the `/ws` WebSocket for the realtime stream.

## File layout

| File         | Purpose                                                        |
|--------------|----------------------------------------------------------------|
| `index.html` | Markup. Three-pane chat shell + auth card + modals + toast.    |
| `style.css`  | Dark theme, brand gradient `#6366f1 → #a855f7`, 8 px radius.   |
| `api.js`     | `fetch`-based HTTP wrapper. Carries `Authorization: Bearer`.   |
| `snaplink_sdk.js` | Snaplink SDK browser adapter (`login`/`postLogin`/`postMFAComplete`/`postToken`). |
| `snaplink_auth.js` | SDK-backed login for the configurable Aero-owned login page. |
| `ws.js`      | WebSocket client with exponential-backoff reconnect + ping.    |
| `render.js`  | Safe DOM rendering — escaping, avatars, blocks, toasts.        |
| `app.js`     | Wiring. State, event handlers, optimistic sends, history.      |

The whole thing is plain ES2020+ modules. No build step. No dependencies.

## Running locally

Serve the directory with any static server. For convenience:

```bash
cd web
python3 -m http.server 8080 --bind 127.0.0.1
```

Now open `http://127.0.0.1:8080/`.

### Talking to the backend

The client calls the API on **the same origin** it was served from. There are
two normal setups:

1. **Backend serves the static files** — drop `web/` behind the Rust app and
   expose it at `/`. `/api/*` and `/ws` already resolve correctly.
2. **Reverse-proxy in front** — run nginx (or `vite`, `caddy`, …) and proxy
   `/api/*` and `/ws` to the Rust process, while serving `web/` as static.

A minimal nginx snippet (assuming the backend listens on `:8000`):

```nginx
server {
  listen 8080;
  root /path/to/aero-im/web;
  location /api/ { proxy_pass http://127.0.0.1:8000; }
  location /ws   {
    proxy_pass http://127.0.0.1:8000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
  }
}
```

If you really need cross-origin access without a proxy, set
`API_BASE` / `WS_BASE` overrides via `localStorage` (see "Quick tweaks" below)
or run the backend with permissive CORS — the client deliberately uses
relative URLs so it stays portable.

## Usage

1. The auth screen follows `GET /api/auth/config`: `both` shows both options,
   `snaplink` shows the Snaplink hosted page, and `local` keeps Aero's page but
   validates credentials through the Snaplink SDK. When Snaplink is configured,
   the Aero-owned form in `both` mode also uses that SDK; registration is then
   managed by Snaplink and its local registration tab is hidden.
2. **Register** on the auth screen (email + display name + password ≥ 6) only
   when the legacy local Aero flow is enabled without Snaplink.
3. After a successful register/login, the access token, refresh token, and
   participant id are persisted in `localStorage` under
   `aero_token`, `aero_refresh`, `aero_pid`.
4. **Create a room** via the "+ 新建" button — pick `group` / `channel`
   / `direct`, optionally name it.
5. **Add members** with the room header button. Paste a participant ID
   (you can register a second user in another browser to grab theirs).
6. **Send messages** — Enter to send, Shift+Enter for newline.

WebSocket frames go through `/ws?token=…`. Browsers can't attach custom
headers to WS, so the JWT rides on the query string — matching the backend
contract.

## Key behaviours

- **Optimistic send.** Your own message renders immediately as a faded
  bubble. When the server echoes it back over WS, the placeholder is
  replaced by the canonical message (matched by sender + text + ≤ 15 s).
- **Exponential backoff.** Reconnect attempts wait `1s, 2s, 4s, 8s, 16s, 30s`
  and cap at 30 s. The dot in the header turns green / amber / red.
- **History on scroll-to-top.** Scrolling within ~40 px of the top fires a
  `?before=<oldest-id>&limit=100` fetch and prepends results while
  preserving the visible scroll anchor.
- **Auth recovery.** A 401 on any API call clears local session and bounces
  back to the login screen.
- **XSS safety.** Every user-controlled string is inserted via `textContent`
  or `escapeHtml`; the only `innerHTML` writes are static skeletons with
  zero interpolation.

## Quick tweaks

| What                      | How                                           |
|---------------------------|-----------------------------------------------|
| Force re-login            | DevTools → `localStorage.clear()` and reload  |
| See WS frames             | DevTools → Network → WS                        |
| Brand colors              | `style.css` — `--brand-1`, `--brand-2`         |
| Backoff schedule          | `ws.js` — `BACKOFF_MS`                         |
| Default history page size | `api.js` / `app.js` — both default to 100      |

## Caveats

- This is a **debug client**: no read-receipts, no editing/deleting, no
  attachments, no notifications.
- Pending messages are not persisted; refreshing the page drops them.
- Presence is whatever the server pushes — no client-side heartbeat
  beyond a 25 s `{"type":"ping"}` to keep the socket warm.
