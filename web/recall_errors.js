// recall_errors.js — pure mapping from recall API failures to toasts.
//
// Kept module-local (no DOM imports, no side effects) so it is unit-testable
// under node:test with plain objects (render_recall.test.js precedent).
//
// Wire contract: recall failures reuse the generic error envelope — HTTP 409 +
// {"code":"conflict","msg":"conflict: <detail>"} (the thiserror Display prefix,
// see crates/aero-common/src/error.rs). The mapping WHITELISTS silence: only the
// two server-produced convergent literals (already recalled / already deleted)
// map to null; EVERY other 409 — window-expired, a reworded window detail, or
// an unknown future variant — reaches the user (error-recovery: 409 must never
// be silently treated as success).

/// Returns `{ text, type }` for the toast to show, or `null` when the failure
/// is a convergent terminal state (already recalled / already deleted — the
/// desired end state is achieved and the UI should stay silent).
export function recallErrorToast(err) {
  if (err?.status === 429) return { text: '撤回太频繁,请稍后重试', type: 'error' };
  if (err?.status === 409) {
    const msg = err?.message || '';
    if (/recall window expired/.test(msg)) {
      // No hardcoded duration in the copy: the window is operator-tunable
      // (AERO_RECALL_WINDOW_SECS) and 0 = unlimited.
      return { text: '撤回时间窗已过,仅作者可在窗口内撤回', type: 'info' };
    }
    // Whitelist: only the two convergent literals stay silent. Anything else
    // (reworded detail, unknown variant, transient race) surfaces — fail-open.
    if (/message is already recalled|message is deleted/.test(msg)) return null;
    return { text: `撤回失败:${msg || '未知错误'}`, type: 'error' };
  }
  return { text: `撤回失败:${err?.message || '未知错误'}`, type: 'error' };
}
