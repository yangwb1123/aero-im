// Pure mapping from recall API failures to user-facing notices. Both the Solid
// shell and the legacy migration-reference adapter use this same contract.
export function recallErrorToast(err) {
  if (err?.status === 429) return { text: '撤回太频繁,请稍后重试', type: 'error' }
  if (err?.status === 409) {
    const msg = err?.message || ''
    if (/recall window expired/.test(msg)) {
      // The recall window is operator-tunable; do not bake a duration into copy.
      return { text: '撤回时间窗已过,仅作者可在窗口内撤回', type: 'info' }
    }
    // Only known terminal conflicts converge silently; unknown 409s surface.
    if (/message is already recalled|message is deleted/.test(msg)) return null
    return { text: `撤回失败:${msg || '未知错误'}`, type: 'error' }
  }
  return { text: `撤回失败:${err?.message || '未知错误'}`, type: 'error' }
}
