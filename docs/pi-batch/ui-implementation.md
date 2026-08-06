# Composer Drafts (草稿持久化) — Implementation Report

Feature: wire server-persisted composer drafts into the web SPA. Autosave per
room (debounced), restore on room switch/reload, clear on send, localStorage
mirror for failed flushes, stale-response protection.

## completion_report

```yaml
completion_report:
  summary: >
    web/drafts.js (400 lines) implements the drafts module: 800ms-debounced
    per-room autosave to GET/PUT/DELETE /api/rooms/:id/draft, restore
    precedence local > server > mirror, per-room op serialization,
    inputRev stale-response guard, mention blocks round-trip, send clears
    draft. Wired via initDrafts DI into app.js switchRoom (delivery.js
    pattern) + index.html module script. 20 plain-node tests.
  changed_files:
    - web/drafts.js
    - web/drafts.test.js
    - web/api.js
    - web/app.js
    - web/index.html
    - web/render.js
    - web/style.css
    - web/package.json
  commands_executed:
    - command: "bash scripts/web-check.sh"
      result: passed
    - command: "node web/drafts.test.js"
      result: passed
    - command: "python /home/u1/ai-batch-runner/scripts/check-frontend-quality.py --dir /home/u1/aero-im/web --json"
      result: passed
    - command: "cargo check --workspace --all-targets"
      result: passed
    - command: "bash scripts/truth-check.sh"
      result: passed
  not_executed:
    - check: real-browser E2E draft flow (type → switch room → restore)
      reason: no browser harness in this environment; covered by 20 unit
        tests + web-check import/syntax gate + API contract tests
  residual_risks:
    - draft restore on tab crash relies on the localStorage mirror; a
      concurrent edit from another device can be overwritten by the local
      mirror replay (documented precedence is deliberate)
    - mentions round-trip via ULID regex mirror of composeBlocksFromInput;
      a future block-type change must update drafts.js too
  assumptions:
    - one draft per (participant, room); server upserts
    - draft is text-only (blocks preserved verbatim for mention round-trip)
```

## 设计要点（与 plan 对照）

- **状态模型**（ui-specs/engineering/async-data.md 决策表）：800ms 防抖 PUT；
  每房间严格串行（操作链在该房间 inflight 尾部，陈旧 PUT 不会落在 DELETE 之后）；
  `inputRev` 计数器防陈旧响应覆盖新状态。
- **恢复优先级**：本地未发送文本 > 服务端草稿 > localStorage 镜像；切换房间时
  先 flush 旧房间再恢复新房间；发送成功清除草稿。
- **失败路径**：401/403/409 稳定映射；flush 失败受镜像保护，服务端无草稿时重放
  并重新保存；restore 后 2s 提示条。
- **模块模式**：仿 delivery.js 的 DI 注入（`initDrafts(state, els)`），drafts.js
  不顶层 import context.js，纯函数可 node 直测；app.js switchRoom 末尾调用
  `draftRoomSwitched(roomId)`。
- **顺带修复**：web/render.js 遗留 `console.log`（uiquality 基线 1 违规）已清除。
