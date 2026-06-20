# Skill: Rust Module Organization

**用途**：在 Aero IM crate 内部正确组织模块结构，保持文件尺寸可控。

---

## Aero IM 的模块组织规范

### 当文件扩展到多大时应拆分

| 文件类型 | 拆分线 | 依据 |
|---|---|---|
| 纯数据结构（`model.rs`, `ids.rs`） | 1200 行 | 数据模型天然扁平，但超 1200 行应拆为 `model/` 子目录 |
| 业务逻辑（`service.rs`, `ws.rs`, `routes.rs`） | **600 行** | 达到 600 行就能识别独立子功能 |
| 仓储层（`message.rs`, `workspace.rs`） | 600 行 | CRUD + 搜索 + 清扫天然三个维度 |
| 工具/适配器（`webhooks.rs`, `scim.rs`） | 800 行 | 协议层面的胶水代码，拆开反而增加导航成本 |
| 前端 JS | 600 行 | ES2020 module 天然支持，拆后仍保持页面加载性能 |

### Crate 内的目录结构模板

```
crates/aero-xxx/src/
├── lib.rs              ← 公共 API 重新导出（≤ 200 行）
├── mod.rs              ← 如果 lib.rs 超过 200 行
├── service.rs          ← 主业务逻辑（≤ 600 行）
├── service/
│   ├── mod.rs          ← pub use 重新导出
│   ├── messages.rs
│   ├── reactions.rs
│   └── reads.rs
├── types.rs            ← 本 crate 私有的数据结构
├── config.rs           ← 配置解析
├── error.rs            ← 错误类型
├── test_util.rs        ← 测试辅助函数
├── db_tests.rs         ← 集成测试（根据 AGENTS.md §3.1 保持 #[ignore]）
└── routes.rs           ← 仅当本 crate 提供 HTTP 路由时
```

### 具体的拆分指引

#### 对于 `aero-server/src/routes.rs`（2635 行）

**不拆文件**，但提取 handler：
- `routes.rs` 保留 `.route()` 调用 + 中间件注册 + 短 handler（≤ 5 行）
- 复杂 handler 委托到 `routes/rooms.rs`、`routes/auth.rs`、`routes/streams.rs`

```
// routes.rs — 只做路由注册
.route("/api/rooms", post(routes::rooms::create_room))
.route("/api/rooms/:id/messages", get(routes::rooms::list_messages))

// routes/rooms.rs — handler 逻辑
pub async fn create_room(...) { ... }  // ≤ 50 行
```

#### 对于 `aero-im-core/src/service.rs`（2418 行）

拆为 `service/` 子模块：

```
service/
├── mod.rs              ← pub use, ≤ 80 行
├── messages.rs         ← send_message, edit_message, delete_message
├── reactions.rs        ← add_reaction, remove_reaction, list_reactions
├── reads.rs            ← mark_read, get_unread, read_receipts
├── notifications.rs    ← send_notification, NotifyBatch, push
├── call.rs             ← call_start, call_answer, call_end
└── room.rs             ← create_room, join_room, leave_room
```

`ImService` 结构体留在 `mod.rs`，方法实现在子模块中通过 `impl ImService` 块：

```rust
// mod.rs
pub struct ImService { ... }

// messages.rs
impl ImService {
    pub async fn send_message(&self, ...) -> Result<...> { ... }
}
```

#### 对于 `web/app.js`（2056 行）

按视图拆为 ES2020 module：

```
web/
├── index.html          ← 主入口
├── style.css
├── app.js              ← 引导 + 路由（≤ 200 行）
├── api.js              ← HTTP 客户端（271 行，OK）
├── ws.js               ← WebSocket 客户端（252 行，OK）
├── render.js           ← DOM 渲染（543 行，OK）
└── views/
    ├── auth.js         ← 登录/注册
    ├── room.js         ← 聊天房间
    ├── stream.js       ← 直播
    ├── call.js         ← 通话
    └── settings.js     ← 设置
```

---

## 公共 API 兼容性原则

拆分后必须保持完全向后兼容：

```rust
// 原文件 outer.rs:
pub fn foo() -> Result<()> { ... }

// 拆分后 outer/mod.rs:
mod inner;
pub use inner::foo;  // 调用方 `use crate::outer::foo` 仍有效
```
