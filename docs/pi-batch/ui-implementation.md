# Composer Drafts (草稿持久化) — Implementation Report (gate-fix round)

Feature: server-persisted composer drafts in the web SPA (autosave per room,
restore on switch/reload, clear on send). This round resolves the gate
findings F1–F6 + testing mandates from the adversarial review.

## completion_report

```yaml
completion_report:
  summary: >
    Gate findings fixed: F1 clear-on-send now hooks BOTH send paths
    (submitComposer calls draftComposerCleared after accepted send — Enter
    keydown and button/form paths share submitComposer); F2 keepalive is
    serialized through the per-room inflight chain and mirror is only
    cleared when the restored server source is strictly newer; F3 sticky
    403 cleared on re-grant (setClean resets forbidden); F4/F5/F6 hardened
    (restore-vs-flush ordering, mirror on in-room autosave failure, hint
    skip). Flow-level tests added (serialization, clear-after-inflight,
    403 re-grant, mirror) + API contract cases in api.test.js. drafts.js
    trimmed to 258 lines (<400).
  changed_files:
    - web/drafts.js
    - web/drafts.test.js
    - web/app.js
    - web/api.js
    - web/api.test.js
    - web/index.html
    - web/render.js
    - web/style.css
    - web/package.json
  commands_executed:
    - command: "bash scripts/web-check.sh"
      result: passed
    - command: "node web/drafts.test.js"
      result: passed
    - command: "node web/api.test.js"
      result: passed
    - command: "python /home/u1/ai-batch-runner/scripts/check-frontend-quality.py --dir /home/u1/aero-im/web --json"
      result: passed
    - command: "cargo check --workspace --all-targets"
      result: passed
    - command: "bash scripts/truth-check.sh"
      result: passed
  not_executed:
    - check: real-browser E2E draft flow
      reason: no browser harness in this environment; flow-level node tests
        + web-check import/syntax gate + API contract tests cover the paths
  residual_risks:
    - cross-device concurrent edit can still be overwritten by the local
      mirror replay (documented precedence local > server > mirror)
    - mention round-trip regex mirrors composeBlocksFromInput; block-type
      changes must update drafts.js in lockstep
  assumptions:
    - one draft per (participant, room); server upserts
    - draft is text-only; blocks preserved verbatim for mention round-trip
```

## 门禁发现修复对照

| 发现 | 状态 | 修复 |
|---|---|---|
| F1 clear-on-send 死路（Enter 不触发 form submit） | ✅ | `submitComposer` 内发送接受后调 `draftComposerCleared(roomId)`（app.js 893/910），Enter 与按钮共用该路径；测试 "clear-on-send: discard deletes AFTER any in-flight PUT (Enter + button)" |
| F2 pagehide keepalive 竞态 | ✅ | keepalive 汇入房间 inflight 链；镜像仅在服务端源严格更新时清除 |
| F3 sticky forbidden 重授权 | ✅ | `setClean()` 清除 `room.forbidden`；测试 "permission re-grant: setClean clears forbidden so autosave resumes" |
| F4 restore-vs-flush 竞态 / F5 房内保存失败无镜像 / F6 陈旧提示 | ✅ | 恢复前串行 flush；保存失败写镜像；跳过时清除提示 |
| restore 流程守卫无流级测试 | ✅ | 串行化/清发送/403/镜像等 13+ 流级测试（drafts.test.js） |
| API 契约测试缺失 | ✅ | api.test.js 增加 draft GET/PUT/DELETE + 401/403 映射用例 |
| drafts.js 400 行预算 | ✅ | 精简至 258 行（<400）；test 文件预算同 |
