All research complete. Here is the implementation plan.

---

# Implementation Plan — 草稿持久化前端接线 (Server-Persisted Composer Drafts)

## 0. Requirement assessment (run first, as instructed)

`python /home/u1/ai-batch-runner/pi-batch.py assess "在 web 前端实现草稿持久化组件：编辑器自动保存与恢复，按房间隔离，防抖保存，发送后清除"` → **frontend_ui / demo 档 (规模 S) / risk low / L0_direct 单任务直改**。

- 处方规则:visual-core(**spacing.md + anti-patterns.md**,已读)+ component-spec(已读)。async-data 未强制(刻意克制)但本任务含防抖/竞态,按 `ui-specs/engineering/async-data.md` 决策表执行。
- 评估缺失项(data_source/permission/error_path/acceptance/tech_stack)已从源码补齐:后端契约在 `crates/aero-server/src/drafts.rs` + `crates/aero-storage/src/draft.rs`,已核实(见 §2);前端 tech_stack = 原生 ES2020 module SPA(无 bundler)。
- 确认现状:web/ 零 draft 符号(唯一命中是 canvas.test.js 的 fixture 字符串,无关);app.js 恰在 **1000 行 JS HARD 线**(`scripts/file-size-check.sh`),模块必须外置,app.js 净零改动。

## 1. Page / feature classification

| 维度 | 值 | 依据 |
|---|---|---|
| product_type | To-B 协作 IM(Slack/Lark 类),debug client | design spec §1;web/README.md |
| page_type | 三栏 workspace shell 的主栏聊天页 — composer 表单区(非独立页面) | index.html `#composer` |
| platform | Desktop web,ES2020+,零依赖原生模块,无框架无响应式系统 | web/package.json,web/README.md |
| density | compact — 状态指示为 11–12px 次级文本行,复用 `.composer-network` 布局密度 | style.css tokens 8pt 体系 |
| motion_level | 极低 — 无动画;仅文本切换 + 既有按钮 transition | anti-patterns 视觉类 |
| risk | **low**(纯增量模块、零后端改动、零数据模型改动);主要风险=防抖 PUT/DELETE/GET 竞态 + app.js 行数红线 | 见 §5 竞态防护 |

## 2. Backend contract(已核实,前端以此为准)

`drafts.rs` + `draft.rs`:

- `GET /api/rooms/:id/draft` → `{ "draft": { room_id, blocks: Block[], reply_to?: str, updated_at: rfc3339 } | null }`(无草稿返回 `draft: null`,**不是 404**)
- `PUT /api/rooms/:id/draft` body `{ blocks: Block[], reply_to?: str }` → `{ saved: true }`;upsert 语义(每 (participant, room) 一行,整体替换)
- `DELETE /api/rooms/:id/draft` → `{ deleted: bool }`(幂等)
- `GET /api/drafts` → 跨房列表。**本任务不用**(克制原则,房间列表 draft 徽标不在需求内)
- 错误映射(`aero-common/src/error.rs`):401 未认证 / 403 无房间访问权 / 404 房间不存在 / 400 `Invalid`(blocks 校验失败、reply_to 不在同房或已删除)。**当前无 409 生产者**(`Conflict` 枚举存在但 drafts 路由不产生)——409 处理为需求指定的防御性分支。
- 注意:`PUT` 会校验 `reply_to` 必须指向同房**现存未删**消息 → 恢复草稿时 reply 上下文可能已失效(见 §5 restore 守卫)。

## 3. Module placement & wiring

### 新增文件
- **`web/drafts.js`** — 自包含域模块,`polls.js` 模式:index.html 独立 `<script type="module">`,DOM-ready 自初始化;共享 `state/ws/els` 单例走 `context.js` import;DOM 之外的核心逻辑(blocks↔text、防抖调度器、竞态守卫)写成**可注入时钟/可脱离 DOM 的纯函数并导出**(`canvas_model.js` / `delivery.js` 的测试模式),便于 plain-node 测试。
- **`web/drafts.test.js`** — node 测试(见 §7)。

### 修改文件(逐文件最小 diff)

| 文件 | 改动 | 说明 |
|---|---|---|
| `web/api.js` | +3 方法:`getDraft(roomId)` / `saveDraft(roomId, {blocks, reply_to})` / `deleteDraft(roomId)`(≈12 行;866 行,余量大) | 照抄既有 `encodeURIComponent` + `request()` 风格 |
| `web/index.html` | ① composer 区 `composer-network` 之后加 `<div id="composer-draft-status" class="composer-draft-status" role="status" hidden>`;② 末尾加 `<script type="module" src="drafts.js">` | 与 polls.js/canvas.js 并列 |
| `web/style.css` | +~15 行 `.composer-draft-status`(镜像 `.composer-network`:padding 4–8px、font-size 11–12px、8pt token;成功用 `--ok`/次级色、错误用 `--danger`、禁用魔法间距) | 间距 token 合规 |
| `web/app.js` | **净 0 行**:+1 import(`initDrafts`)、+1 装配调用 `initDrafts({ forceReauth });`(并入 "extracted domain wiring" 段)、`switchRoom` 末行 `restoreAiHistory(roomId);` → 同行追加 `draftRoomSwitched(roomId);`;为保住 ≤1000 HARD 线,合并 wiring 段 2 行注释(净 −2)。**不改 submitComposer** | 见 §4 事件流 |
| `web/context.js` / `web/ws.js` / `web/render.js` | **零改动** | 草稿状态模块内私有,不进共享 `state`(delivery.js 先例) |

### exports(`drafts.js`)
- `initDrafts({ forceReauth })` — 装配入口(app.js 调用一次)
- `draftRoomSwitched(roomId)` — switchRoom 末行调用(flush 旧房 → restore 新房)
- 纯逻辑导出供测试:`draftBlocksToText(blocks)` / `textToDraftBlocks(text)`(mentions 往返)、`createDraftScheduler({now, schedule, fetchImpl})`(防抖/排序/竞态核心,DI 时钟与网络)

## 4. State model & event flow

**状态全在 drafts.js 模块内**(不污染共享 `state`,delivery.js 先例):

```js
// module-local
let activeRoom = null;                       // composer 当前归属房间
const rooms = new Map();                     // roomId -> DraftRoomState
// DraftRoomState = { text, replyTo, dirty, timer, inFlight:Promise, gen,
//                    lastSavedAt, status: 'idle|saving|saved|error|forbidden',
//                    forbidden:bool }
```

**事件流**(async-data 决策表:autosave 属 mutation,防抖仅限输入事件;竞态必须用版本 token 而非 loading 布尔):

1. **autosave(debounced)**:`composerInput 'input'`(drafts.js 自挂监听,与 app.js 监听并存)→ 记 dirty + 捕获快照 `{room: activeRoom, text, replyTo, gen: ++rooms[room].gen}` → 800ms timer(输入类 debounce,async-data §2)。触发时若该房有 in-flight PUT → **不并发发新 PUT,链式排队**(保序,防旧文本后到覆盖新文本);无 in-flight → `PUT`,成功→`status:'saved'` + 记 `lastSavedAt`,失败→`status:'error'`。
2. **flush on switch**(`draftRoomSwitched(newRoom)`,switchRoom 末行调用,此时 `state.currentRoomId` 已是新房,旧房文本仍在 textarea 中):
   a. `prev = activeRoom`;若 prev 有 dirty/待发 → 取消 timer、**立即 flush**(跳过 debounce),同时把快照同步写 localStorage 安全镜像(见 §5);
   b. 清空 textarea、`activeRoom = newRoom`、`gen` 全局推进(作废旧房所有 in-flight 响应);
   c. restore:`GET /api/rooms/:id/draft`(异步)→ 命中且守卫通过(见 §5)→ 填 textarea + 刷新 `composerSend.disabled` + 若 `reply_to` 存在则置 `state.replyTo = {id, sender_id:null, blocks:[]}` 并重绘回复 chip(renderReplyChip 对未知 sender 有 id 前缀兜底)→ 指示器"已恢复草稿"(瞬态)。
3. **clear on send**:drafts.js 自挂 `composer` form `submit` 监听(**在 app.js 的 submitComposer 之后触发**,同事件两监听按注册序执行)。submit 后判 `els.composerInput.value.trim() === ''`:
   - 空 → 发送已受理(成功或已入 delivery.js 离线队列):取消该房 timer、bump gen、若有 in-flight PUT 则**链式等其 settle 后 DELETE**(防 PUT-after-DELETE 复活草稿)、无 in-flight 直接 `DELETE`;本地镜像清除;指示器隐藏。
   - 非空 → sendOptimistically 失败/中止:草稿保留,不动。
   因此 **submitComposer 零改动**。
4. **pagehide / visibilitychange(hidden)**:待发快照用 `fetch(..., {keepalive:true})` flush + 同步写本地镜像(与 delivery.js `pagehide → persistPending` 同构;防刷新丢最后 800ms)。

## 5. Interaction chain & failure paths(含竞态守卫)

**状态机**(async-data §6 写入态):`idle → saving → saved / error / forbidden`,restore 态 `restoring → restored / empty / error`。

| 场景 | 行为 | 规范依据 |
|---|---|---|
| 无草稿 | 指示器 hidden,placeholder 不变(empty 态) | anti-patterns §4(空态齐全) |
| restore 加载中 | 指示器 "正在恢复草稿…"(loading 态) | component-spec §6 |
| restore 命中 | 填 composer + reply chip + "已恢复草稿" 瞬态 | — |
| restore **竞态**:GET-A 慢于用户切到 B | 响应携带请求时 roomId,apply 前校验 `activeRoom === roomId`,否则丢弃 | async-data §3(禁止旧响应覆盖新状态) |
| restore **竞态**:响应到达前用户已开始输入 | `gen` 已推进 / composer 非空 → 放弃恢复(用户文本优先,会随 autosave 覆盖旧草稿) | async-data §3 |
| restore `reply_to` 失效(父消息已删) | 父消息不在 `state.messagesByRoom` 时只恢复文本、丢弃 reply(发送时后端会 400,保留也无益) | drafts.rs reply 校验 |
| PUT 网络错误/5xx | `status:'error'`,指示器 "草稿保存失败,点击重试"(可点击重发);**文本保留、dirty 保留**,下次 input 自动再调度;不无限自动重试 | async-data §5(mutation 不自动重试);component-spec §7(保存失败不得清空表单) |
| **401** | 调注入的 `forceReauth()`(loadHistory 先例) | async-data §5(401 → 刷新令牌后重放;此处会话失效→重登) |
| **403** | 该房标记 `forbidden`:停止一切后续 save(不空转打 403),指示器 "无法保存草稿(无权限)" + toast;**不丢文本** | async-data §5(403 不重试) |
| **409** | 防御分支(后端当前不产生):指示器错误 + 手动重试入口,不自动重试 | 任务要求;async-data §5(409 交给用户决策) |
| flush 失败(切房时) | 文本已从 textarea 清走 → **localStorage 安全镜像** `aero_draft_v1:{pid}:{room}`(同步写,写于 flush 时刻)兜底:下次进该房 restore 时若服务端无草稿且镜像存在 → 恢复镜像文本并**回灌 PUT**(自愈);服务端 save 成功后清镜像 | 防数据丢失(草稿功能的成立前提) |
| DELETE 失败 | 幂等操作,下轮 submit 再试;不阻塞发送 | draft.rs delete 幂等 |

**竞态防护汇总**(全部显式):① per-room 串行化 PUT(保序);② `gen` 版本号作废过期响应(PUT/DELETE/GET 共用);③ discard = cancel-timer + gen-bump + 链式 DELETE-after-in-flight;④ restore 双守卫(room 匹配 + 未输入)。

## 6. Visible draft indicator

`#composer-draft-status`(role="status",置于 composer-network 之下、form 之上):文本 + 颜色双通道(anti-patterns 视觉类"状态只用颜色不用文字"红线):
- `saving`:"草稿保存中…"(次级色)
- `saved`:"草稿已保存 12:03"(`--ok` 或次级色,复用 `formatTime`)
- `restored`:"已恢复草稿"(瞬态 2s 后转 saved/idle)
- `error`:"草稿保存失败,点击重试"(`--danger`,整行可点,重发当前快照)
- `forbidden`:"无法保存草稿(无权限)"

## 7. Change radius & test plan

**改动半径**:新增 2 文件(`drafts.js` 预估 ≤300 行、`drafts.test.js`);修改 4 文件(api.js +12 / index.html +2 / style.css +15 / app.js ±0 净行)。**零后端、零 context.js/ws.js 改动**。drafts.js 行数远低于 JS 1000 HARD 线;app.js 保持 1000。

**测试**(plain node,`node --test`,仿 delivery.test.js/canvas.test.js:MemoryStorage/FakeWs/假时钟注入,不引依赖):

`web/drafts.test.js` 用例:
1. `textToDraftBlocks` / `draftBlocksToText` 往返:纯文本、@mention(25 位 ULID 正则与 app.js `composeBlocksFromInput` 同源)、混合段落——恢复后逐字符一致
2. 防抖调度器(注入假时钟):多次 input 合并为一次 PUT;800ms 内再输入重置 timer
3. flush-on-switch:切房时取消 debounce 立即 PUT;旧房 in-flight 时新 flush 排队不并发
4. discard(发送清空):取消 pending + gen bump + in-flight 链式 DELETE(验证 DELETE 发生在 PUT 之后,无复活窗口)
5. restore 竞态:GET-A 迟到于切房 B → 丢弃;响应到达前已输入 → 不覆盖用户文本
6. 错误映射:401→forceReauth 回调被调;403→forbidden 态且不再发 PUT;409/5xx→error 态 + 手动重试一次
7. 本地镜像:flush 失败后镜像留存,下次 restore 服务端空时回灌并重发 PUT;服务端 save 成功则清镜像
8. 空文本语义:submit 时值为空 → DELETE(草稿清除)

**测试接线**:`web/package.json` `"test"` 脚本追加 `drafts.test.js`(现有 15 个文件同款)。

**验证门禁**(AGENTS.md §4.3):
- `cd web && node --test drafts.test.js`(及全量 `npm test`)
- `scripts/web-check.sh` 0 违规(语法 + import 解析 + index.html 脚本引用存在)
- `scripts/file-size-check.sh` 0 违规(app.js ≤1000、drafts.js 达标)
- `cd web && npm run lint`(若 npm 可用;eslint max-lines 同阈值)
- 活验证(§4.3):throwaway 库 + 前台起 server,双标签:房 A 输入 → 等 1s → 刷新 → 草稿恢复;切房 B 输入 → 切回 A → A 文本仍在;发送 → 重进 A → 无草稿;另一用户进 A → 看不到草稿(私有性)。

**明确不做**(克制):`GET /api/drafts` 跨房列表/房间侧栏草稿徽标、草稿多版本/历史、附件入草稿(media.js 的文件/语音发送不经过 composer 文本,不涉及)、离线草稿队列(本地镜像仅兜底不重放)。

---

**Verdict**: plan is ready — backend contract verified end-to-end (`drafts.rs`/`draft.rs` 路由、错误码、upsert/幂等语义),前端按既有 polls.js/delivery.js 模块模式落位,竞态与失败路径按 async-data 决策表显式设计,app.js 行数红线有净零方案,测试计划覆盖防抖/竞态/错误映射核心逻辑。
