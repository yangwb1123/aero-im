# 架构评估：跨验证分析与 Aero IM 客户端基础设施缺口

> **视角**：资深架构师 | **分析对象**：「第六轮全局扫描——5 个客户端基础设施方向」的交叉验证报告
> **代码基线**：当前工作树（`session.rs` Wave 15 已合并，包含 `/api/auth/refresh`）

---

## 前置事实核查：交叉验证中的关键不准确性

在进入架构分析之前，必须先修正交叉验证报告本身的一组事实错误——这些错误会影响对方向一（token 生命周期）的评估结论。

### 修正：`/api/auth/refresh` 端点**确实存在**

交叉验证报告称：

> 无 `/api/auth/refresh` 路由 | ✅ 确认——routes.rs 有注释 `// Session management: access-token refresh` 但无对应 POST 路由

**实际代码验证**：

| 证据点 | 路径 | 状态 |
|--------|------|------|
| `routes.rs` `.merge(crate::session::routes())` | 行 289 | ✅ 已挂载 |
| `session.rs` `route("/api/auth/refresh", post(refresh))` | 行 24 | ✅ 端点存在 |
| `session.rs` `route("/api/auth/logout", post(logout))` | 行 25 | ✅ 端点存在 |
| 服务端 refresh 实现 | 完整实现含 rotation + reuse detection + family revocation | ✅ 5 次提交历史 |

**后端实现质量评估**：`session.rs` 中的 refresh 实现实际上相当完善——包含：

1. **签名验证优先**：先验证 JWT 签名和 `kind == Refresh`，再查吊销表——防伪造 token 的 DoS
2. **Token rotation**：每次 refresh 颁发全新 access+refresh 对，旧 refresh token 立即进黑名单
3. **Reuse detection + grace window**：已轮换的旧 token 被重放时，10 秒内视为良性重试（丢响应重发），超过则触发**全局会话族吊销**（OWASP refresh-token-rotation reuse detection 的最佳实践）
4. **Session 审计**：`session.revoked` 最佳努力审计事件（带 hash 前缀，不暴露完整 hash）
5. **与 PAT / bot token 共存**：`AuthUser` extractor 的级联验证链（JWT → PAT → bot token）已包含 refresh token 验证路径

### 真正的缺口不在服务端，在客户端

后端基础设施已经就绪。**真正的架构缺口是**：

```
后端已建：JwtCodec.issue(kind=Refresh) + AuthService.refresh() + session.rs HTTP 端点
                 + token rotation + reuse detection + family revocation + session audit
                                    ↓
客户端断链：auth.getRefresh() 存储在 localStorage 中从未使用
                                    ↓
没有任何 401 拦截器 → 不会自动调用 POST /api/auth/refresh → access token 1h 后全部 401
                                    ↓
ws.connect(token) 取的是 connect 时刻的快照，永不更新
                                    ↓
token 过期后 WS 重连会携带过期 token → 被服务器拒绝
```

这不是"代码不存在"的问题，而是**架构上服务端的投入与客户端的断链**。这意味着后端所有关于 refresh token rotation、reuse detection、family revocation 的安全投资——在当前的客户端架构下——**完全没有发挥作用**。

---

## 1. 架构评估

### 1.1 当前架构的优势

| 维度 | 优势 |
|------|------|
| **后端认证管线** | 分层验证链（JWT → PAT → bot token），每个层级有独立的结构化前缀检查（`is_well_formed_pat` / `is_well_formed_bot_token`），在哈希和 DB 查询之前线性过滤恶意 bearer |
| **JWT 实现** | RS256 + 显式 `kid` + 零停机密钥轮换（`from_pems` 的 extra 验签密钥环）+ `jti` 唯一化同秒颁发 + leeway=0 的过期验证 |
| **Token rotation** | OWASP 级 refresh token rotation，含 grace window 和跨会话族吊销 |
| **Session 生命周期** | 从登录（`record_session`）到 logout（`revoke_by_hash`）到审计（`audit_session_revoked`）的完整链路 |
| **密钥轮换** | 支持零停机密钥轮换——新密钥签名 + 旧密钥留在验签环中直到所有 token 过期，`kid` 选择验签密钥 |
| **离线退化** | PAT 和 bot token 在 localStorage 清空后仍可用（硬编码或外部存储），与 JWT 的 refresh 机制互为补充 |

### 1.2 关键架构设计缺陷

#### 缺陷 1：客户端 token 生命周期——服务端完备，客户端真空

这是**交叉验证完全错过了真正问题**的典型。交叉验证"确认"了 refresh 端点不存在，但问题的本质不是端点缺失，而是架构上**服务端与客户端之间存在一条断裂的信任链**：

```
服务端认知：客户端有 refresh token → 会在 access token 过期前调用 refresh → 保持会话
                    ↓
客户端行为：localStorage 存储 refresh token → 从未读取调用
                    ↓
实际结果：access token 过期 → 全部 401 → 用户在 1 小时后被强制登出
```

**严重程度评估**：

| 后果 | 影响 |
|------|------|
| 生产环境 1 小时会话断裂 | 所有活跃用户每 1 小时经历一次全 API 失败——除非手动重新登录 |
| WS 重连死亡 | 网络抖动后重连携带过期 token → WS 连接被服务器拒绝 → 永久断连直到用户手动刷新页面 |
| 安全性徒增复杂度 | refresh token rotation / reuse detection / family revocation → 代码复杂度增加但效果为 0（因为从未调起） |
| session 记录膨胀 | 服务端为每个过期/不用的 refresh token 记录 session，无人清理 |

**根本原因**：`web/api.js` 的 `request()` 函数（约行 41-85）没有任何响应拦截器来处理 401。一个 `fetch` 的响应拦截器是 REST 客户端的标准模式（Angular `HttpInterceptor`、Axios `response.use`），但其缺失意味着系统无法在 token 过期时静默续期。

#### 缺陷 2：WS 认证的静态快照

`ws.js` 行 82-93：

```javascript
connect(token) {
  this.token = token;     // 静态快照，永不更新
  ...
  let url = `${proto}//${location.host}/ws?token=${encodeURIComponent(this.token)}`;
  // 重连时也用同一个 token
}
```

WebSocket 连接在建立时认证一次。但 WS 连接的生命周期可能远超过 access token 的 TTL（默认 1 小时）。问题：

- **无 WS 层 token 刷新协议**：没有标准方式在已建立的 WS 连接上更换 token（除非断开重建）
- **重连 token 过期**：断线重连时，如果 access token 已过期，重连会使用过期 token → 服务器拒绝
- **无心跳重认证**：没有 WS 帧级别的 `auth_renew` 消息协议

#### 缺陷 3：跨 Tab 的 N 倍放大效应

每个浏览器标签页建立独立的 WS 连接 + 独立的 access token。在 token 生命周期管理已断裂的情况下，N 个标签页 = N 倍的 401 失败体验 + 服务端 N 倍的扇出流量。

### 1.3 架构债务

| 债务类型 | 具体表现 | 修复成本 |
|----------|---------|---------|
| **安全幻觉** | 后端 refresh token rotation 投入了大量工程，但因客户端断链而未生效 | 中（客户端添加 401 拦截器） |
| **连接浪费** | 无 leader election → N 标签页 = N WS 连接 | 高（需 BroadcastChannel + 状态重设计） |
| **状态真空** | 纯内存状态 → 刷新即空 | 中（IndexedDB 分层缓存） |
| **代码冗余** | 3 个独立 token helper（`generate_token`/`hash_token` 在 `webhook`/`scim`/`invitation` 中重复） | 低（提取共享模块） |
| **本地存储 XSS 风险** | access token 和 refresh token 均在 localStorage 明文存储 | 低（httpOnly cookie 替代方案/加密） |

---

## 2. 扩展方向

从架构角度，以下 5 个方向按高价值排序。注意这些方向与交叉验证报告中的 5 个方向**有重叠但也有重大差异**——我基于架构影响面重新组织。

### 方向一（P0 · 架构断裂修复）：客户端 Token 生命周期管线

> **这是当前系统最严重的架构断裂——服务端完备 + 客户端真空，导致整个认证基础设施失效**

#### 为什么需要

- **安全性归零**：后端投入的 token rotation / reuse detection 完全不发挥作用
- **生产可用性断裂**：用户每 1 小时被强制登出一次
- **WS 连接脆弱**：网络抖动后的重连被过期 token 拒绝
- **技术指标**：当前每用户每小时产生（access TTL 次）×（API 调用次数）次的 401 失败

#### 核心挑战

1. **fetch 无原生拦截器**：`fetch` API 不像 Axios 有 `response.use()` 拦截器——需要在每个调用点或一个包装函数中实现 refresh-on-401
2. **并发 refresh 竞态**：多个 API 调用同时返回 401 时，不能并行发出多个 refresh 请求——需要锁（或 `Promise` 共享）
3. **WS token 更新**：WS 连接建立后无法更换 token——需要设计 WS 协议帧 `auth_renew` 或断开重连策略
4. **refresh token 也会过期**：refresh token 也有 TTL（默认 7 天或更长），过期后只能重定向到登录页

#### 预期的架构变更

```diff
web/api.js:
+  let refreshPromise = null;  // 正在进行的 refresh 请求的 Promise
   async function request(...) {
     // ... 现有请求逻辑
     if (!resp.ok) {
+      if (resp.status === 401 && auth.getRefresh()) {
+        // 尝试静默续期（带竞态保护）
+        if (!refreshPromise) {
+          refreshPromise = refreshTokens();
+        }
+        const newToken = await refreshPromise;
+        // 重试原请求（用新 token）
+        headers['Authorization'] = `Bearer ${newToken}`;
+        resp = await fetch(url, { ... });
+        // 继续返回
+      }
       // ... 原有错误处理
     }
   }

web/ws.js:
+  // 添加 auth_renew 消息处理
+  // 或：在重连前检查 token 是否过期，过期则先刷新再连
   connect(token) {
+    this.token = token;
+    // 启动定时器，在 token 过期前触发更新
+    this._renewTimer = setInterval(() => {
+      // 若 token 即将过期，请求新 token 并通知服务器
+    }, ACCESS_TTL * 0.8);  // 在 80% TTL 时刷新
     ...
   }
```

#### 对现有系统的影响

- **向后兼容**：refresh API 已存在，纯客户端改造，不影响任何后端
- **无迁移成本**：用户浏览器 token 数据天然持久
- **风险**：refresh 可能失败（refresh token 被吊销），此时需要平滑降级到登录页

---

### 方向二（P1 · 架构优化）：跨 Tab 连接协调与状态同步

#### 为什么需要

- **WS 成本**：N 个标签页 = N 条 WS 连接 = N 倍服务端 `mpsc` + `fan_out_raw` = N 倍内存
- **UX 不可信**：未读计数在不同标签页中不一致——用户在 Tab A 阅读后，Tab B 仍显示红点
- **通知轰炸**：一条消息到达 → N 个标签页各弹一条桌面通知
- **行业对标**：Slack 2018 年引入 BroadcastChannel 协调；Discord 有类似机制

#### 核心挑战

1. **Leader election**：选出一个"主标签页"持有 WS 连接，其他标签页休眠——leader 崩溃后自动转移
2. **BroadcastChannel 兼容性**：Safari 15.4+ 才支持，需要 `localStorage` `storage` 事件作为 fallback
3. **版本偏差**：不同标签页可能加载了不同版本的 JS（懒加载/缓存过期不一致），需要协商协议版本
4. **状态合并**：未读计数、已读游标等状态需要跨标签页合并而非覆盖

#### 预期的架构变更

```
新模块 web/tab-coordinator.js:
├── BroadcastChannel('aero-im-coord')
├── Leader election (first-to-connect / heartbeat-based)
├── 消息类型:
│   ├── leader_announce { pid, ts }
│   ├── read_advance { room_id, last_message_id }
│   ├── notification_seen { notification_id }
│   └── state_invalidate { room_id }
└── 降级策略 (storage 事件 → localStorage 写入)
```

#### 对现有系统的影响

- **中等侵入性**：需要重构 WS 单例从 `context.js` 的 `export const ws = new WsClient()` 到 `TabCoordinator` 托管的工厂方法
- **后端无变化**：纯前端改造
- **服务端正效应**：N 标签页 → 1 条 WS 连接，减少服务端内存和扇出

---

### 方向三（P1 · 架构完整性）：SPA 状态分层持久化

> 交叉验证报告将此列为独立方向，但架构上它与方向二（跨 Tab 协调）是**强耦合的**——持久化是跨 Tab 协调的前提，没有持久化就没有可合并的状态基线。

#### 为什么需要

- **页面刷新即空**：F5/Cmd+R 后一切归零——房间列表、未读计数、当前房间、草稿全部丢失
- **5-10 次 API 调用恢复**：刷新后必跑 `refreshRoomsFromServer()` + `rtcConfig()` + `liveGifts()` + `refreshNotifBadge()` + `loadHistory()` + `reactionsBatch()` + `listReceipts()`
- **对标产品都有缓存**：Slack 本地 SQLite、Discord 缓存频道列表、Telegram Web IndexedDB

#### 核心挑战

1. **存储分层策略**：
   - 会话级（`sessionStorage`）：当前房间 ID、滚动锚点、AI 上下文
   - 本地级（`localStorage` 或 `IndexedDB`）：房间列表缓存、未读计数、草稿
   - 缓存级（`IndexedDB` + stale-while-revalidate）：最近 N 个房间的消息历史
2. **多标签页写入竞态**：N 个标签页同时写入同一 IndexedDB 对象存储——需要事务化合并策略
3. **存储配额**：IndexedDB 通常 50MB-无上限，需要 LRU 淘汰 + 按房间分库
4. **状态版本化**：服务端 schema 变更后，缓存的旧结构需要失效

#### 预期的架构变更

```
web/cache-store.js:
├── 三层抽象 (SessionStore / LocalStore / CacheStore)
├── IndexedDB schema:
│   ├── rooms_cache (房间元数据)
│   ├── messages_by_room (最近消息，分房间)
│   ├── unread_counts (未读计数，跨 Tab)
│   ├── drafts (草稿)
│   └── state_version (缓存失效标记)
└── stale-while-revalidate 读取模式
```

#### 对现有系统的影响

- **中高侵入**：`context.js` 的状态初始化函数 `enterChat()` 需要插入缓存还原逻辑
- **后端无变化**：纯前端改造
- **风险**：缓存数据 >= 5 分钟时，可能显示过时数据——需要明确的 staleness 指示器

---

### 方向四（P1 · 安全架构）：凭据与密钥生命周期管理

> 这是交叉验证报告中方向一的扩展——不仅修复客户端 refresh 断链，还要从根本上解决凭据的存储和管理架构。

#### 为什么需要

- **JWT 签名私钥在 Git 仓库中**：`secrets/jwt_private.pem` 被 Git 跟踪——仓库暴露一次即整个认证体系失守
- **无凭据轮换基础设施**：从创建至今未轮换，也无轮换 API
- **无凭据存储抽象**：凭据来源散落在环境变量、配置文件、文件系统三处

#### 核心挑战

1. **密钥旋转的零停机**：`JwtCodec::from_pems` 已有 extra 验签密钥环支持——但缺少生产级轮换流程（自动化、过期告警、旧密钥清理）
2. **凭据来源统一**：需要 `SecretsProvider` trait——环境变量、Vault、AWS Secrets Manager 等来源可切换
3. **refresh token 的对后兼容**：旧 refresh token 在新密钥下需要继续被验签直到过期
4. **CI 流水线中的凭据注入**：CI 测试需要凭据但不应写入 repo

#### 预期的架构变更

```diff
crates/aero-auth/src/
+  secrets.rs:
+  pub trait SecretsProvider {
+    fn get(&self, key: &str) -> Option<String>;
+  }
+  pub struct EnvSecretsProvider;  // 环境变量来源
+  pub struct FileSecretsProvider; // 文件系统来源（当前行为）
+  pub struct VaultSecretsProvider; // Vault/HashiCorp 来源（未来）

aero-auth/src/jwt.rs:
   JwtCodec::from_pem -> 改为从 SecretsProvider 读取
   添加 key rotation schedule 支持
```

#### 对现有系统的影响

- **中等侵入性**：重构配置加载，但 JWT 模块已有 `from_pems` 的支持，轮换逻辑已存在
- **向后兼容**：默认行为保持文件/环境变量读取
- **风险**：无——迁移可以增量进行

---

### 方向五（P2 · 产品化/Accessibility & i18n）：企业合规与全球化基础

> 交叉验证将此拆为 a11y/i18n/主题三个方向，但架构上它们共享同一套基础设施——一个**可扩展的样式 + 国际化系统**。

#### 为什么需要

- **企业合规门槛**：ADA/EU 无障碍法规逐步在软件采购中强制执行——零 a11y = 企业销售天花板
- **全球化市场**：所有 UI 硬编码英文 + 无 RTL 支持 = 无法进入非英语市场
- **品牌定制**：无主题系统 = 无法为品牌客户（OEM/白标）提供差异化

#### 核心挑战

1. **从零构建 a11y**：ARIA landmarks、焦点管理（TAB 序列）、屏幕阅读器（role/label）、键盘快捷键、对比度——全部从零构建，无法渐进增强
2. **i18n 管线**：字符串提取 → JSON 翻译 → 运行时切换（`Intl` API 日期/数字本地化）——需要工具链支持
3. **CSS 变量主题**：`render.js` 中的 CSS 变量已存在但未用于运行时切换——需要 `document.documentElement.style.setProperty` 架构

#### 预期架构变更

```
web/i18n.js:
├── 翻译加载器 (fetch → JSON → 缓存)
├── t() 函数 (模板字符串替换)
├── Intl.DateTimeFormat / Intl.NumberFormat 包装
└── RTL 方向检测和切换

web/theme.js:
├── CSS 变量覆盖机制 (setProperty)
├── Theme 枚举 (light / dark / high-contrast)
├── localStorage 持久化 + BroadcastChannel 同步
└── prefers-color-scheme 媒体查询自动检测

render.js:
+ role, aria-*, tabindex 属性注入
+ focus() / blur() 管理到所有交互式元素
```

#### 对现有系统的影响

- **高侵入性**：几乎每个 `render.js` 中的 HTML 模板都需要修改以注入 ARIA 属性
- **后端无变化**：纯前端改造
- **迁移策略**：建议从"合规最低要求"（landmarks + 键盘导航）起步，逐步扩展到 i18n 和主题

---

## 3. 接口设计建议

### 3.1 关键接口设计原则

| 原则 | 适用的方向 | 说明 |
|------|-----------|------|
| **1. 渐进增强** | 方向二/三/五 | 新功能应"先行后治"——先保证功能可用，再逐步增强体验。持久化和跨 Tab 协调不应阻塞页面渲染 |
| **2. 失败降级** | 所有方向 | 每个新抽象层的失败不应拖垮主功能。BroadcastChannel 不可用 → 退化为单 Tab 模式。IndexedDB 满 → 退化为纯内存。refresh 失败 → 引导用户重新登录 |
| **3. 后端免责** | 方向一到五 | 所有客户端扩展应**零后端变更**。后端已是完备的——所有缺口在前端 |
| **4. 状态可序列化** | 方向三 | 所有 `context.js` 中的 Map 状态都应能被序列化到 IndexedDB——这意味着从 `Map<MessageId, Message>` 迁移到可 JSON 序列化的对象 |

### 3.2 新抽象层

#### 必要性评估

| 抽象层 | 方向 | 是否必要 | 替代方案 |
|--------|------|---------|---------|
| **Token 生命周期管理器** (`token-manager.js`) | 方向一 | **必须** | 无——当前代码完全无此概念 |
| **Tab 协调器** (`tab-coordinator.js`) | 方向二 | **必须** | 放弃多 Tab 协调（但不是 IM 的可选方案） |
| **缓存存储** (`cache-store.js`) | 方向三 | 建议 | localStorage 直接操作（不够结构化） |
| **SecretsProvider trait** | 方向四 | 建议 | 保持现状（增加了运维复杂度） |
| **国际化管线** (`i18n.js`) | 方向五 | **必须** | 硬编码英文（但 Sales 无法进入非英语市场） |
| **键盘导航管理器** (`keyboard.js`) | 方向五 | 建议 | 非强制（重度用户临时解决方案） |

### 3.3 向后兼容性策略

| 改造 | 兼容策略 |
|------|---------|
| 方向一：401 拦截器 | 纯新增逻辑，旧 token 行为不变；refresh 失败降级到现有错误处理 |
| 方向二：TabCoordinator | `context.js` 的 `ws` 单例改为工厂模式，初始 `export const ws = new WsClient()` 不变，但内部视 leader 状态决定是否真的建连 |
| 方向三：持久化 | `enterChat()` 增加缓存还原步骤，但始终先显示内存状态，后台持久化完成后合并 |
| 方向五：a11y | 纯新增 ARIA 属性，对现有功能零影响——屏幕阅读器用户获得更好的体验，非屏幕阅读器用户无差别 |

---

## 4. 技术选型

### 4.1 前端新技术引入评估

| 技术 | 方向 | 评估 | 优先级 |
|------|------|------|--------|
| **BroadcastChannel API** | 方向二 | 浏览器原生 API，零额外依赖，Chrome 54+/Firefox 38+/Safari 15.4+——兼容性门槛较低 | P0 — 立即使用 |
| **IndexedDB**（raw API 或 `idb-keyval`） | 方向三 | 浏览器原生 API。直接使用 IDB 事务模型（不加 Dexie 等重量库），保持零依赖策略 | P0 — 立即使用 |
| **IntersectionObserver** | 消息导航（方向五扩展） | 浏览器原生 API，用于实现"新消息指示器"和"日期分隔符"等 UX 增强 | P1 — 建议使用 |
| **Web Locks API** | 方向三（IndexedDB 写入竞态） | 浏览器原生 API，Chrome 69+/Firefox 65+/Safari 15.4+——用于跨 Tab 的 IndexedDB 写入协调 | P2 — 可选，可用时间戳合并替代 |
| **SharedWorker** | 方向二（替代方案） | 比 BroadcastChannel 重量级，更复杂的生命周期管理。仅在需要真正的共享状态（非仅消息）时使用 | P2 — 不推荐（BroadcastChannel 足够） |
| **Service Worker** | PWA/离线 | 当前系统的独立缺口。sw.js 注册 + 缓存策略 + 离线队列 | P2 — 独立于方向二/三，需要单独分析 |

### 4.2 零额外依赖原则

Aero IM Web SPA 目前是零依赖的纯 ES2020 应用（除 `eslint` 外无 npm 依赖）。架构决策应坚持这一原则：

| 功能 | 零依赖方案 | 引入依赖的代价 |
|------|-----------|---------------|
| 401 拦截器 + refresh | 纯 `fetch` 包装函数（20 行以内） | 引入 Axios 会增加约 14KB gzip，且与现有 `fetch` 不兼容 |
| IndexedDB 缓存 | 原生 IndexedDB API（或 `< 1KB` 的 `idb-keyval`） | Dexie 约 12KB，但功能冗余 |
| 翻译/国际化 | JSON 翻译文件 + 运行时 `Intl` API | `i18next` 约 8KB，可以接受但非必须 |
| 键盘快捷键 | 原生 `keydown` 事件监听 | `hotkeys-js` 约 1KB，可以接受 |
| 拖放上传 | 原生 `dragenter/dragover/drop` 事件 | `dropzone.js` 约 30KB，过重 |

**建议**：坚持零额外依赖原则。所有需求都可以用浏览器原生 API 在 20-100 行 JS 内实现。

### 4.3 服务端侧新技术引入评估

| 技术 | 方向 | 评估 |
|------|------|------|
| **AWS Secrets Manager / HashiCorp Vault** | 方向四（凭据管理） | 需要 Rust SDK 集成，对应 crate 已有。Vault 更复杂，建议从环境变量 + 文件系统起步，逐步演进 |
| **临时 Token 签发 API** | 方向一（WS 短期 token） | 不需要新依赖——当前 JWT 系统已经支持 `TokenKind::Access` 的任意 short TTL |

---

## 5. 实施路线图

### 优先级排序

```
P0 (立即) ─── 方向一：客户端 token 生命周期管线修复
                ├── 401 拦截器 + 静默 refresh（2-3 天）
                └── WS token 快照 → 定时刷新（3-5 天）

P1 (近周) ─── 方向二：跨 Tab 协调
                ├── BroadcastChannel 协调层（3-5 天）
                └── Leader election + WS 单例化（5-8 天）
               
P1 (近周) ─── 方向三：SPA 状态分层持久化
                ├── IndexedDB schema + 会话还原（5-7 天）
                └── RDS read-through 缓存（3-5 天）
                
P1 (近周) ─── 方向四：凭据生命周期
                ├── JWT 密钥移出 Git（1 天）
                └── SecretsProvider trait + 轮换（5-7 天）

P2 (月内) ─── 方向五：a11y + i18n + 主题
                ├── ARIA landmarks + 键盘导航（5 天）
                ├── i18n 管线 + 首个翻译集（7 天）
                └── CSS 变量主题系统（3 天）
```

### 阶段划分

#### 阶段 1：止血（P0 · 3-5 天）

**目标**：修复 token 生命周期断裂——这是生产就绪的硬门槛。

**交付物**：
1. `api.js` 中增加 `response interceptor`，在收到 401 时自动调用 `POST /api/auth/refresh`
2. `ws.js` 中增加 `tokenRenew()` 方法，在重连前检查 token 过期状态并刷新
3. 竞态保护：多个并发 API 调用只触发一次 refresh

**里程碑**：用户打开标签页后可用 >8 小时而不被 401 登出。

**风险**：
- Refresh token 过期 → 必须重定向到登录页（重新登录）
- Refresh API 返回非 200 → 保持原有错误行为，然后降级到登录页

#### 阶段 2：基础设施（P1 · 2-3 周）

**目标**：跨 Tab 协调 + 状态持久化，解决核心 IM 体验问题。

**交付物**：
1. `TabCoordinator` 模块（BroadcastChannel + storage 事件 fallback）
2. Leader election 逻辑
3. WS 单例化（非 leader Tab 降级为 passive 模式）
4. `CacheStore` 模块（IndexedDB 三层存储）
5. `enterChat` 增加缓存还原

**里程碑**：刷新页面后，用户状态（当前房间、未读计数）自动恢复；多标签页间未读计数一致。

**风险**：
- BroadcastChannel Safari <15.4 不支持 → storage 事件 fallback 延迟较高
- IndexedDB 写入竞态（多 Tab 同时写）→ 可能需要 Web Locks API 或乐观合并
- 缓存数据过时 → 需要明确的 staleness 指示器

#### 阶段 3：安全深度（P1 · 1-2 周）

**目标**：凭据管理架构升级。

**交付物**：
1. JWT 私钥移出 Git（`secrets/` 目录 → `.gitignore` + env 覆盖）
2. `SecretsProvider` trait + 环境变量/文件系统两套实现
3. 密钥自动轮换文档化

**里程碑**：CI/CD 中不需要 `secrets/` 目录即可部署；密钥可在零停机下轮换。

**风险**：
- 密钥轮换期间，过期 token 的验签问题 → 已由 `from_pems` 的 extra 验签环解决
- 开发环境密钥需重新生成 → 影响所有开发者的 `.env` 文件

#### 阶段 4：产品化（P2 · 3-4 周）

**目标**：企业合规 + 全球化。

**交付物**：
1. ARIA landmarks 注入到所有核心视图
2. TAB 键导航 + 焦点管理
3. `i18n.js` 管线 + 中英双语翻译集
4. Dark Mode + 高对比度主题

**里程碑**：可通过基本的 ADA/EU 合规扫描；非英语市场客户可以使用母语上线。

**风险**：
- ARIA 注入量大——涉及 `render.js` 中 ~30+ HTML 模板
- 翻译集维护需要产品经理/翻译人员参与
- RTL 布局在纯 CSS 下可能触发布局重排

### 总体风险评估

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| 方向一：refresh token 颁发失败（服务端过载） | 用户被强制登出 | 低 | 降级到登录页，显示友好错误 |
| 方向二：BroadcastChannel 不支持 | 跨 Tab 功能退化 | 中（Safari <15.4） | storage 事件 fallback |
| 方向三：IndexedDB 写入竞态 | 状态丢失或冲突 | 中 | 乐观合并 + 最后写入者胜出 |
| 方向三：缓存数据过时 | 用户看到旧数据 | 中 | stale-while-revalidate + 指示器 |
| 方向五：RTL 布局重排 | 视觉不一致 | 低 | CSS 逻辑属性（`inset-inline-start` 等） |

---

## 总结

### 交叉验证报告的独特贡献（修正后）

1. **方向一的 token 生命周期缺口分析**虽然是正确的方向，但其"无 `/api/auth/refresh` 路由"的验证结论是**错误的**——端点存在且实现完备。真正的问题是客户端**不使用**这个端点。这一区别很重要：修复方案不是"加端点"而是"修客户端"。

2. **方向二的跨 Tab 协调分析**是可靠的——3-5 条独立 WS 连接确实会造成 3-5 倍服务端成本和差的 UX。

3. **方向三/四/五的覆盖声明需要修正**——交叉验证报告自身指出方向二至五已被既有分析覆盖，但未深入评估这些既有分析的实际质量。我的架构分析确认既有覆盖是系统性的但**缺乏实施路线图**——这正是本报告提供的增量价值。

### 最关键的一句话

> **Aero IM 的认证架构存在一个"后端投入巨大 + 客户端完全断链"的断裂**。服务端有 RS256 JWT、refresh token rotation、reuse detection、family revocation、session audit、PAT、bot token——这是一个企业级认证系统的完整投资。客户端在 localStorage 中存储 refresh token 但从未使用它。修复这一断裂（3-5 天工作）应优先于所有其他客户端改进，因为它是当前架构中唯一真正将后端安全投资**无效化**的瓶颈。
