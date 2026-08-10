# Composer Drafts 前端接线 — 实现计划 v3（产物格式修复轮）

## 0. 本轮定位（先读，这是与之前轮次的本质区别）

需求评估已跑：frontend_ui / demo 档（S）/ risk low / L0_direct；处方 specs 已读并应用。**功能代码已完成且在树中、全部门禁通过**——两轮 implement 的 `VALIDATION_FAILED (exit=1)` 不是代码问题，是**产物格式问题**。已从 `.pi-batch/rejected/ui-implementation.md.7p47qrn3-20260806-044148.rejected.md` 逐字核实：implement 阶段验证器是 `check-completion-report.py`，它校验的是 **agent 最终消息**（被捕获为 {output}），要求消息内含 fenced YAML `completion_report` 块；前两轮最终消息是散文总结（无 YAML 块）→ 验证器 exit=1 → 产物被拒。**本轮 implement 的任务 = 复跑门禁 + 用正确格式收尾，不是重写代码。**

## 1. 页面/功能分类

| 维度 | 值 |
|---|---|
| product_type | IM SPA 聊天主视图的 composer 子功能 |
| page_type | 主视图内嵌组件（`#composer` form + `#composer-draft-status` 状态条） |
| platform | Desktop web 优先（textarea Enter 发送），兼容移动浏览器 |
| density | medium（8pt token：状态条 padding 8px 16px / font-size 12px，已合规） |
| motion_level | low（纯文字状态、零动画；文字+颜色双通道） |
| risk | low→medium（私有数据写、无越权面；历史 F1 曾致已发送文本复活为草稿，已修） |

## 2. 树状态（本轮已实跑核实）

- **文件**：`web/drafts.js`(244) + `web/drafts_store.js`(293，纯状态) + `web/drafts.test.js`(397) + `web/drafts_restore.test.js`(125) + `web/drafts_test_helpers.js`(78)；改动 `web/api.js`(+getDraft/saveDraft/deleteDraft)、`web/api.test.js`(391，+3 契约测试)、`web/app.js`(1000，+draftComposerCleared 钩子)、`web/package.json`；前轮已接 `auth_ui.js`/`index.html`/`style.css`/`render.js`
- **门禁复跑全绿**：web-check 0 违规 · drafts+restore+api 42/42 · npm test 116/116 · ws.test.js 16/16 · eslint 0 问题 · uiquality 0 违规 · file-size exit 0 · truth-check 0 ORPHAN · cargo check 干净
- **v2 计划修复全部落地**：F1 clear-on-send 挂 `submitComposer` 单一漏斗（Enter keydown 与 submit 按钮都经它，`app.js:893/910` 两路径清空后调 `draftComposerCleared`）；F2 keepalive 经 `store.afterInflight` 链入 per-room inflight、`pickRestoreAction` 数值时间戳决策（mirror 更新则胜出，仅 server 真正更新或 clean 保存确认才清 mirror）；F3 `setClean` 解除 forbidden；F4/F6 restore 守卫纯函数化 + 输入竞态 skip 时清除陈旧提示；F5 保存失败写 mirror（store 拥有 mirror 生命周期）；行数预算全 ≤400

## 3. 模块放置（现状，勿改结构）

- `drafts_store.js`：纯逻辑（blocks 转换、`createDraftStore`、`pickRestoreAction`、`resolveReplyTarget`、`shouldDiscardOnSend`、mirror 读写清）——node 可测
- `drafts.js`：DOM 胶水（`initDrafts` DI、`draftRoomSwitched`、`draftComposerCleared`、`resetDrafts`、restoreRoom、指示器、pagehide keepalive）——app.js 经 `initDrafts({state, els, forceReauth, clearReply, renderReplyChip})` 注入（delivery.js 先例）
- `ws.js` 零改动（草稿纯 REST）；后端零改动（`/api/rooms/:id/draft` 已核实吻合）

## 4. 状态模型与交互链（已实现，§2 门禁覆盖）

- 状态全在模块内：per-room `Map`（text/replyTo/dirty/inputRev/timer/inflight/status/forbidden/pendingDelete）+ `inflight` 串行链 + `inputRev` 版本守卫（async-data 决策表）；restore 优先级 local > mirror(比 server 新) > server > mirror > empty；写入态 idle→saving→saved/error/forbidden
- 失败路径：401→forceReauth；403→forbidden 停存不丢文本（setClean 解除）；409/5xx/网络→dirty 保留 + 指示器点击重试 + mirror 兜底；restore 竞态→双守卫 skip；flush 失败→mirror 回灌自愈；DELETE 幂等
- 指示器：保存中/已保存 HH:MM/恢复中/已恢复 2s/失败可点/无权限

## 5. 本轮 implement 任务清单（按序执行）

1. 复跑 §2 门禁命令（只验证；真红才改代码，改完重跑至绿）
2. **最终消息 = 交付物**：结尾必须含 fenced YAML `completion_report` 块，机械要求（check-completion-report.py）：
   - `commands_executed` 非空，每项 `{command, result}`，result ∈ {passed, failed, not_executed}；`changed_files` 非空
   - `not_executed` 每项带 `reason`（浏览器 E2E、多标签并发冲突——如实写，环境无浏览器 harness）
   - `residual_risks`/`assumptions` 如实（多设备 last-writer-wins、keepalive 硬卸载 best-effort、mention 正则双份、app.js 钩子无静态门）
   - 禁止「理论上应该通过/should pass/assume passed」类伪造措辞；措辞避开拒绝标记词
   - 消息主体紧凑（有输出上限），YAML 块放结尾；`docs/pi-batch/ui-implementation.md` 照写一份同内容作仓库文档（但验证只看最终消息）
3. 不做：不重写代码、不拆模块、不新增功能、不提交 git、不动无关文件

## 6. 验收标准

- 门禁复跑全绿（§2 矩阵）
- 最终消息含合规 YAML completion_report → `check-completion-report.py` exit=0
- 无 git commit、无无关文件改动

**改动半径**（本轮）：0 代码改动预期；唯一产出是正确格式的最终消息 + 复跑证据。**测试计划**：无需新增测试（42 个草稿/恢复/契约测试 + 116 全量已覆盖）；若复跑发现真红则修到绿。
