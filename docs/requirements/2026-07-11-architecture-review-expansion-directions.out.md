These are valuable corrections to validate against the actual codebase before publishing the review. The document is well-structured, but two specific claims don't hold up to code verification:

---

## Correction 1: Message Edit Race (already solved)

**Review claim**: `edit_message` does `SELECT ... FOR UPDATE` but isn't row-locked → concurrent edits can silently overwrite.

**Actual code** (`crates/aero-storage/src/message/crud.rs`, migration 0157):

```rust
// Optimistic locking via `version` column
pub async fn edit(&self, id: MessageId, blocks: Vec<Block>,
    expected_version: i32, // ← caller reads this first
) -> Result<Option<Message>, aero_common::Error> {
    let row = sqlx::query_as::<_, MessageRow>(
        r#"UPDATE messages SET blocks = $1, ..., version = version + 1
           WHERE id = $4 AND deleted_at IS NULL AND version = $5  ← atomic guard
        RETURNING ..."#,
    ).bind(...).bind(expected_version).fetch_optional(&self.pool).await?;

    match row {
        Some(r) => Ok(Some(r.into())),
        None => {
            // distinguish missing/deleted vs version mismatch
            if exists { Err(Error::Conflict("message was edited concurrently; reload and retry")) }
            else { Ok(None) }
        }
    }
}
```

`ImService::edit_message` calls `get_version()` then passes it to `edit()`. This is textbook optimistic concurrency control — the UPDATE atomically increments `version` and the WHERE clause acts as a compare-and-swap. A concurrent edit returns `Conflict`, not silent overwrite. The message_history table captures both versions on top of that.

**Verdict**: Claim is inaccurate. The race is already handled. The "~5 lines in `message_edit.rs`" fix is already in place.

---

## Correction 2: innerHTML XSS in web SPA (already defended)

**Review claim**: `render.js` uses `innerHTML` extensively for message rendering → "single largest security surface in the entire product" → `Block` model has no sanitization seam.

**Actual code** (`web/render.js`):

```javascript
// Line 3 comment:
// "All user-controlled strings are inserted via textContent or element attributes,
//  never via innerHTML. The few innerHTML usages below are TEMPLATE STRINGS
//  WITH NO INTERPOLATION (static skeleton); user content is filled in afterwards
//  using textContent / setAttribute."
```

Every block type handler builds DOM via `el()` (which uses `document.createElement` + `.textContent =`):
- `'text'`: `el('span')` + `span.textContent = content`
- `'code'`: `el('pre')` + `code.textContent = b.content`
- `'mention'`: `el('span')` + `tag.textContent = @name`
- `'file'` (image): `el('img')` + `attrs.src = url`, `alt = b.name` (attribute, not innerHTML)
- `'card'`: `el('div')` + `textContent`
- `'button'` with URL: `safeUrl` checked against `/^https?:\/\//i`
- All links: `target='_blank' rel='noopener noreferrer'`

The `appendTextWithSpans` function navigates byte-offset rich text with DOM nodes, never string interpolation.

**Verdict**: The codebase is already ahead of this concern. The `innerHTML` comment is accurate — the few uses are static skeleton templates with no interpolation. The CSP recommendation is still valid defense-in-depth, but the framing as an XSS vulnerability is not supported.

---

## What stands up well

The other three directions and the remaining edge cases hold up better:

| Claim | Status |
|---|---|
| No CSP/HSTS/X-Frame-Options headers | ✅ Confirmed — no security headers middleware |
| No per-endpoint granular rate limiting | ✅ Confirmed — rate limiter is WS-message-scoped only |
| Webhook secret rotation endpoint missing | ✅ Confirmed — no POST `/api/webhooks/:id/rotate-secret` |
| PAT audit trail missing | ✅ Confirmed — no last-used/IP logging on PAT |
| `rotate_key` not row-locked | ⚠️ Partial — the UPDATE is atomic but two concurrent calls can return different keys. The last write wins, so it's safe for the DB, but callers may get a stale key back. Minor issue. |
| Blob GC crash window | ✅ Confirmed — delete-then-ack pattern, crash between delete and ack = blob lost |
| Migration partial failure | ✅ Confirmed — `sqlx::migrate!` per-migration tx, non-idempotent retry blocks startup |
| K8s/Helm/CI-CD gaps | ✅ Confirmed — no deployment automation at all |
| Media pipeline fragmentation | ✅ Confirmed — three independent intake paths, no shared `MediaSource` trait |
| Multi-tenant cost attribution | ✅ Confirmed — per-tenant metrics gated behind default-off flag |

---

Want me to verify any of the other specific claims against the actual source? Or would you like help turning any of the valid findings into implementation tickets with exact file anchors?
