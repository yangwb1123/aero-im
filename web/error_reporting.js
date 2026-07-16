// error_reporting.js — global unhandled-rejection reporting.
//
// Extracted out of app.js (file-size-check.sh's 1000-line hard cap) rather than
// left inline — a single listener with no state, so it needs no access to
// app.js's other bootstrap machinery beyond the `toast` renderer.

/// Install a window-level `unhandledrejection` handler: logs the error and
/// shows a toast so the user knows something went wrong, rather than failing
/// silently (10th analysis). `toast` is injected (from render.js) to avoid a
/// circular import back into app.js.
export function installUnhandledRejectionReporting(toast) {
  window.addEventListener('unhandledrejection', (ev) => {
    console.warn('unhandled rejection:', ev.reason);
    const msg = ev.reason?.message || String(ev.reason || 'unknown error');
    // Only toast for non-abort errors (timeouts are expected on slow networks).
    if (msg !== 'The user aborted a request.') {
      toast(`请求失败: ${msg}`, 'error');
    }
  });
}
