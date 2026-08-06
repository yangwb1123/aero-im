Assessment loaded and tree state verified. Here is the implementation plan.

---

# Implementation Plan — 草稿持久化前端接线 (Server-Persisted Composer Drafts)

## 0. Requirement assessment & current tree state

`python /home/u1/ai-batch-runner/pi-batch.py assess ...` → **frontend_ui / demo 档 (S) / risk low / L0_direct**。处方:visual-core(spacing.md + anti-patterns.md,已读)+ component-spec(已读)。评估缺失项(data_source/permission/error_path/acceptance/tech_stack)由源码补齐(见 §2、§5)。

**⚠️ 工作树现状(必须纳入计划)**:本任务上一个 implement 阶段**超时中断**,但改动已在工作树中(未提交、未验证):

```
 M web/api.js  M web/app.js  M web/index.html  M web/package.json  M web/render.js  M web/style.css
?? web/drafts.js(451 行, 10 exports)  ?? web/drafts.test.js(20 tests)
```

- app.js 仍 **1000 行**(净零行约束已达成:import L35、`draftRoomSwitched(roomId)` 同行追加 L553、`initDrafts({...})` L965、wiring 段注释合并 −2 行)。
- 上次中断点:最后一个命令 `node --test drafts.test.js` 未返回结果 → **实现已落盘但门禁从未跑过**。本计划即该实现的规格;implement 阶段 = 按此计划核对/修正现有文件 + 跑通全部门禁(§7)。

## 1. Page / feature classification

| 维度 | 值 | 依据 |
|---|---|---|
| product_type | To-B 协作 IM(Slack/Lark 类),debug client | design spec §1;web/README.md |
| page_type | 三栏 workspace shell 主栏聊天页的 composer 表单区(非独立页面) | index.html `#composer` |
| platform | Desktop web,ES2020+,零依赖原生 ESM,无框架/无响应式系统 | web/package.json |
| density | compact — 指示器 12px 次级文本行,复用 `.composer-network` 密度 | style.css tokens |
| motion_level | 极低 — 无动画,仅文本切换 | anti-patterns 视觉类 |
| risk | **low**(纯增量、零后端改动);风险集中在防抖 PUT/DELETE/GET 竞态 + app.js 1000 行红线 + uiquality 阈值 | §5、§7 |

## 2. Backend contract(已核实,以源码为准)

`crates/aero-server/src/drafts.rs`(grep 确认路由)+ `crates/aero-storage/src/draft.rs`:

- **`GET /api/rooms/:id/draft`** → `{ "draft": { room_id, blocks: Block[], reply_to?: str, updated_at: rfc3339 } | null }`(无草稿 = `draft: null`,非 404)
- **`PUT /api/rooms/:id/draft`** body `{ blocks: Block[], reply_to?: str }` → `{ saved: true }`;upsert 语义(每 (participant, room) 一行整体替换)
- **`DELETE /api/rooms/:id/draft`** → `{ deleted: bool }`(幂等)
- `GET /api/drafts` → 跨房列表。**本任务不用**(克制,房间列表徽标不在需求内)
- 错误映射(`aero-common/src/error.rs`):401 未认证 / 403 无房间访问(`assert_room_access`)/ 404 房间不存在 / 400 `Invalid`(blocks 校验失败、reply_to 非同房现存未删消息)。`Conflict`(409)枚举存在但 drafts 路由当前不产生 → **409 为需求指定的防御性分支**。
- **注意**:任务描述里的 `PUT /api/drafts/:room_id` 是简写,实际路由是 `/api/rooms/:id/draft`(已按源码对齐)。
- `PUT` 校验 reply_to 必须指向同房现存未删消息 → restore 时 reply 上下文可能已失效(§5 守卫)。

## 3. Module placement & wiring

### 新增文件
- **`web/drafts.js`**(已在树,451 行)→ 自包含域模块:index.html 独立 `<script type="module">` + DOM-ready 自初始化(polls.js 模式);**不 import context.js 顶层**(store/blocks/restore-source 纯逻辑须 plain-node 可测)→ `state`/`els` 经 `initDrafts` 注入(delivery.js DI 模式,与 polls.js 的差异在模块头注释说明)。
- **`web/drafts.test.js`**(已在树,20 tests)→ node:test + assert/strict,FakeClock/FakeNet(deferred 手动控制)注入。

### 修改文件(均在树,逐文件最小 diff)

| 文件 | 改动 | 状态 |
|---|---|---|
| `web/api.js` | +3 方法:`getDraft(roomId)` / `saveDraft(roomId, {blocks, reply_to})` / `deleteDraft(roomId)`(插在 `// ----- polls -----` 前) | ✅ 已改 |
| `web/index.html` | ① `composer-network` 后加 `<div id="composer-draft-status" class="composer-draft-status" role="status" hidden>`;② 末尾加 `<script type="module" src="drafts.js">` | ✅ 已改 |
| `web/style.css` | +`.composer-draft-status`(padding 8px 16px、font-size 12px、`--ok`/`--danger`/`--text-mute`,全部 8pt token 集内;`.error` 可点) | ✅ 已改 |
| `web/app.js` | **净 0 行**:import(L35)、`initDrafts({ state, els, forceReauth, clearReply, renderReplyChip })`(L965)、`switchRoom` 末行 `restoreAiHistory(roomId); draftRoomSwitched(roomId);`(L553);wiring 段 2 行注释合并 −2 行。**submitComposer 零改动** | ✅ 已改,仍 1000 行 |
| `web/render.js` | 删 `toast()` 里 `console.log` 兜底(改为 `if (!stack) return;`)— **uiquality ERROR 类违规修复**(该行是本次触碰文件中唯一的 error 类违规) | ✅ 已改 |
| `web/package.json` | `"test"` 脚本追加 `drafts.test.js` | ✅ 已改 |
| `web/context.js` / `web/ws.js` | **零改动**(草稿状态模块内私有,不进共享 `state`;发送流程经 submit 监听观测,ws.js 无涉) | — |

### drafts.js exports(已核实)
`initDrafts(deps)`、`draftRoomSwitched(roomId)`(app.js 用)+ 纯逻辑:`textToDraftBlocks` / `draftBlocksToText`(mention 正则与 app.js `composeBlocksFromInput` 同源,`lastIndex` 显式重置)、`resolveRestoreSource`、`createDraftStore({now, schedule, clear, save, del, onStatus, onAuthError})`、`mirrorWrite/Read/Clear`(localStorage 镜像,导出供测试)。

## 4. State model & event flow

**状态全在 drafts.js 模块内**(不进共享 `state`,delivery.js 先例):

```
module-local: activeRoom / store / statusEl / runtime(deps) / restoreFailed / restoredHintTimer
rooms: Map<roomId, { text, replyTo, dirty, inputRev, timer, inflight, status,
                    lastSavedAt, forbidden, pendingDelete }>
状态机(async-data 写入态): idle → saving → saved / error / forbidden;restore 态 restoring → restored / empty / error
```

**事件流**(async-data 决策表:autosave=mutations,防抖仅限输入事件;竞态用版本 token,不靠 loading 布尔):

1. **autosave(800ms debounce)**:drafts.js 自挂 `composerInput 'input'`(与 app.js 监听并存,app.js 先挂先执行)→ `store.input(roomId, text, replyTo)` 捕获快照 + `inputRev++` + 调度 timer。触发时若有 in-flight PUT → **链式排队**(`room.inflight` 串行化,保序,杜绝旧 PUT 晚于 DELETE 落地复活草稿)。
2. **flush on switch**(`draftRoomSwitched(newRoomId)`,switchRoom 末行,此时新房已激活、旧房文本仍在 textarea):取 `store.snapshot(prev)` → **同步写 localStorage 镜像**(`aero_draft_v1:{pid}:{room}`)→ 立即 `store.flush(prev)`(跳过 debounce;失败 rethrow → toast「草稿保存失败,已保留在本地」,文本由镜像兜底)→ 清空 textarea/回复 chip/指示器 → `restoreRoom(newRoomId)`。
3. **restore**:`GET` → 双守卫(**房间未变** + **`inputRev` 未变**=期间无输入)→ `resolveRestoreSource` 优先级 **本地未存文本 > 服务端草稿 > 本地镜像 > 空**;服务端草稿命中 → 填 textarea + 刷新 send 按钮 + 若 `reply_to` 的父消息在 `state.messagesByRoom` 中 → 置 `state.replyTo` + `renderReplyChip()`(缺失则丢弃 reply 上下文);镜像命中 → 填文本 + `input`+`flush` 自愈回灌。
4. **clear on send**:drafts.js 自挂 `composer 'submit'` 监听(**在 app.js submitComposer 之后执行**——同事件监听按注册序,app.js 先注册)。submit 后 `value.trim()===''` 且房间有状态 → `store.discard(roomId)`(cancel timer + inputRev++ + 链式 DELETE-after-in-flight)+ `mirrorClear`;发送失败(文本仍在)→ 不动。**submitComposer 零改动**(已核对其 markdown/blocks 两路径成功后均清空输入框)。
5. **pagehide / visibilitychange(hidden)** → `mirrorWrite` + `fetch(..., {keepalive:true})` PUT(delivery.js pagehide 先例;防刷新丢最后 800ms)。

## 5. Interaction chain & failure paths

| 场景 | 行为 | 依据 |
|---|---|---|
| 无草稿 | 指示器 hidden,placeholder 不变(empty 态) | anti-patterns §4 |
| restore 中 | 「正在恢复草稿…」(loading 态) | component-spec §6 |
| restore 命中 | 填 composer + reply chip(父消息存活才保留)+「已恢复草稿」瞬态 2s | §4.3 |
| restore 竞态(切房/输入) | inputRev + activeRoom 双守卫,过期响应丢弃 | async-data §3 |
| PUT 网络错/5xx/409 | `status:'error'` 指示器「草稿保存失败,点击重试」;**文本保留、dirty 保留**;不自动重试(409 防御分支同) | async-data §5;component-spec §7(失败不清空) |
| 401 | `forceReauth()`(注入;loadHistory 先例) | async-data §5 |
| 403 | 房间标 `forbidden`:停止自动保存(输入仍本地跟踪)、指示器「无法保存草稿(无权限)」+ toast;**不丢文本** | async-data §5(403 不重试) |
| flush 失败(切房) | 镜像同步写入先行 → 下次进房 restore 时服务端空 → 镜像回灌 + 自愈重存;保存成功/删除后清镜像 | 防数据丢失(草稿功能成立前提) |
| DELETE 失败 | 幂等;`pendingDelete` 标记,下轮 submit / 指示器重试再跑 | draft.rs delete 幂等 |
| 指示器 | `#composer-draft-status` role="status",文本+颜色双通道:保存中/已保存 HH:MM(`--ok`)/恢复中/已恢复/失败可点(`--danger`)/无权限 | anti-patterns 视觉类(状态不只靠颜色) |

**竞态防护汇总**(全部显式):① per-room 串行化链(`enqueue` 尾永不 reject,失败不破链);② `inputRev` 版本号——保存成功仅当期间无新输入才清 dirty、restore 应用仅当期间无输入;③ discard = cancel-timer + inputRev++ + 链式 DELETE-after-in-flight;④ 镜像同步写于 flush 之前。

## 6. Change radius & test plan

**改动半径**:新增 2 文件(drafts.js 451 行 / drafts.test.js 20 tests);修改 6 文件(api.js +12 / index.html +2 / style.css +12 / app.js ±0 / render.js −1 行 console.log / package.json +1 token)。零后端、零 context.js/ws.js。

**⚠️ uiquality 门发现(需 implement 阶段处理)**:`drafts.js` **451 行 > 400 行 WARNING 阈值**(strict 模式计入失败;仓库既有 57 条同类违规,app.js 1000/api.js 866 均超)。处理选项(按序):(a) 精简注释/合并状态行压到 ≤400(优先,约省 60 行);(b) 若压不下去,接受为与仓库基线同类的 warning 并在 completion_report 如实列出。**禁止**为消警告拆分模块(破坏单一职责,违背克制原则)。其余阈值已控:api 调用 4 ≤ 5、嵌套深度 ≤ 4、无 console.log/debugger/skip/eslint-disable/innerHTML、空 catch 均为 `catch { /* 注释 */ }` 或带 body(规避 `_SWALLOWED_RE` 的 `catch(...) {}` 形态)。

**测试**(plain node,`node --test web/drafts.test.js`,FakeClock + FakeNet 手动 deferred,零依赖):已落盘 20 用例覆盖——① blocks↔text 往返(mentions 首/中/尾/相邻、短 @词不误判、regex lastIndex 跨调用重置);② 防抖合并(400+400 不触发、800 触发一次);③ flush 立即保存 + 无残留定时器;④ per-room 串行化(新 PUT 等 in-flight);⑤ 慢保存不清 dirty(rev 守卫);⑥ discard 链式 DELETE-after-PUT、无 timer 竞态;⑦ discard 失败 → retry 重跑 DELETE;⑧ 清空输入框 → DELETE 不 PUT 空;⑨ 401→onAuthError;⑩ 403→forbidden 停存不丢文本;⑪ 5xx→error 保留 dirty + retry 重存;⑫ flush 失败保留 dirty(镜像前提);⑬ restore 守卫原语(inputRev/setClean/snapshot/hasState);⑭ 优先级 local>server>mirror>empty;⑮ 镜像写读清/per-room/畸形 payload。

## 7. Gates(implement 阶段必须实跑)

```
cd /home/u1/aero-im && bash scripts/web-check.sh          # 语法 + import 解析 + index.html 脚本引用
cd /home/u1/aero-im/web && node --test drafts.test.js     # 新增
cd /home/u1/aero-im/web && node --test ws.test.js         # 任务指定(未触碰,回归)
cd /home/u1/aero-im/web && node --test api.test.js        # api.js 被触碰 → 回归
cd /home/u1/aero-im/web && node --test delivery.test.js delivery_cursor.test.js  # 依赖链回归
cd /home/u1/aero-im && bash scripts/file-size-check.sh    # app.js ≤1000、drafts.js ≤1000
cd /home/u1/aero-im && bash scripts/truth-check.sh        # Rust 死代码门(未触碰,回归)
cd /home/u1/ai-batch-runner && python scripts/check-frontend-quality.py --dir /home/u1/aero-im/web --strict 2>&1 | grep -E "drafts|render.js"  # 触碰文件无新增 error 类;render.js console.log 已消除
```

活验证(如环境允许):throwaway 库起 server,双标签验「输入→刷新→恢复」「切房隔离」「发送→清除」「他人不可见草稿(私有性)」。

**明确不做**(克制):`GET /api/drafts` 列表/房间侧栏徽标、草稿多版本、附件入草稿(media.js 文件/语音发送不经过 composer 文本)、离线重放队列(镜像仅兜底不重放)。

**残留风险(诚实列出)**:drafts.js 451 行超 400 行 warning 阈值(见 §6);409 分支为防御性(后端当前不产生);restore 与 GET 竞态窗口内「发送空表单」理论边缘(草稿内容仍在 textarea,可接受);app.js/api.js 既有 god-file warning 属仓库基线、不在本次范围。
