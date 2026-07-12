好，我已经完整地审阅了这份分析文档，并与代码库进行了交叉验证。以下是我的评估：

---

## 整体评估

**总体评语**：一份卓越的战略分析——几乎所有核心论点都得到了底层代码证据的确凿支持。四个方向均验证为真实存在、可触及且彼此独立。然而，文档中存在以下**事实性不准确之处**，需要在推进前纠正。

---

## 已确认：方向一——发送侧可靠性

**核心论点已验证**：`send_message` 帧确实**没有 nonce**，`send()` 仅检查 WebSocket 是否打开，`Error` 帧仅 `toast()` 且**不链接回对应的 pending 消息**。然而：

### 🔴 文档中的不准确之处

> 声称 `findPendingMatch` 使用 `JSON.stringify` 进行内容匹配

**实际代码**（`web/app.js` 第 473-483 行）：使用的是 `textOf()`——该函数仅**拼接纯文本块**（`type === 'text'`），忽略提及、代码块、附件等。表述得好像错误更严重（匹配整个 JSON），但实际抽象泄漏更微妙：相同文本但不同提及的消息会被视为重复消息。

> 断言客户端没有超时回滚机制

**实际上已存在部分机制**：`findPendingMatch` 采用 **15 秒时间窗口**（第 481 行：`Math.abs(sT - pT) <= 15000`）。匹配并非开放式的——但该窗口没有 UI 反馈。超时后，pending 消息会**永远滞留在屏幕上**，无任何错误指示。文档提出的「超时→变灰+错误」建议**正确**，但初始前提（零超时机制）存在偏差。

### 经验证的漏洞（真实且可复现）

**内容匹配 + 15 秒窗口漏洞**：`findPendingMatch` 中缺失 `textOf(serverMsg.blocks) !== textOf(pending.blocks)` 检查。如果用户在 15 秒内发送两条 `textOf` 输出相同的消息，第二条服务端确认消息会错误地指向第一条 pending 消息的 DOM 节点，而第二条 pending 消息则成为**幽灵消息**。这是一个可通过 `textOf` 变体（提及 `@id` 会出现）触发的真实错误——但分析中提到的 `JSON.stringify` 变体在 `textOf` 下不会出现（提及会被忽略）。

### 需要修订的评估

| 方面 | 文档称 | 实际 |
|------|--------|------|
| 匹配机制 | `JSON.stringify` 整块 | `textOf()` 仅拼接纯文本 |
| 超时窗口 | 零超时/无限 pending | 15 秒时间窗口，无 UI 反馈 |
| 幽灵消息漏洞 | 发送两次「收到」触发 | 在 `textOf` 下可触发，但提及不再引发问题 |

**建议**：将「P0. 添加 nonce 到 `send_message`」升级为**关键修复**——它同时解决了匹配问题和 15 秒窗口问题。当前的 15 秒窗口具有误导性：它给人一种「存在过载保护机制」的错觉，但在配置不当的浏览器时钟下（`Date.parse` 基于客户端时间戳），它完全不可靠。

---

## 已确认：方向二——Web Push

**全局搜索** `VAPID`、`WebPush`、`pushManager`、`serviceWorker`、`ServiceWorker`——**零结果**。该断言被完美证实。

### 推送给关架构已准备就绪

`PushGateway` trait（`aero-push/src/lib.rs` 第 99 行）是一个极简特化 trait：

```rust
pub trait PushGateway: Send + Sync {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError>;
}
```

`build_push_gateways()`（`helpers.rs` 第 37-59 行）目前构造：

```rust
aero_server::state::PushGateways { fcm, apns }
```

**只需添加三个组件**：`WebPushGateway` 实现 `PushGateway`（包装 `web-push` crate）、注入到 `build_push_gateways()`、以及一个 `'web'` 平台分支到 `PushGateways::for_platform()`。token 注册/撤销 API 已存在（`push_token.rs`）。文档中「方向二独立，无需前置」的判断完全正确。

---

## 已确认：方向三——媒体面韧性

### SfuMediaSession 跑循环（`sfu_media.rs` 第 112-160 行）

**无 ICE restart**。跑循环通过 `poll()` → `Timeout` → `select!` → `handle_datagram()` → 失败时直接 `return`：

```rust
Err(e) => {
    warn!(error = %e, peer = %self.peer.id(), "sfu-media: poll error");
    return;
}
```

没有重试，没有 `DtlsReconnect`，没有 `IceRestart`。文档中「ICE 断开 → 直接退出」的断言**被完全证实**。

### HLS 完整性缺失

`HlsWriter`（`aero-live-hls/src/lib.rs` 第 47-103 行）严格按照片段推送顺序工作——在 `push_segment()` 或 `finish()` 处没有 `0x47` 同步字节验证。`ts.rs` 中定义了一个 `TS_SYNC_BYTE` 常量（`ts.rs` 第 26 行），但仅用于测试。文档关于「推流中断会产生损坏的 TS，且无完整性校验」的断言**完全正确**。

### HlsWriter `finish()` 无清理功能

`finish()` 仅向 manifest 添加 `#EXT-X-ENDLIST`——不清除残留的 `.ts` 文件，不实施任何删除策略。

### 看门狗不存在

**零命中**。无 `stream_watchdog` 出现——文档的 P0 建议（60 秒 tick 扫描过时直播）**已验证为缺失**。

### 轻微不准确

> 断言「SRT 在底层 UDP socket 断开后也不会尝试重新握手」

**部分不准确**：作为推流端（非服务端）的 SRT 固有协议在应用层包含**自身的重连握手**（HSv5 handshake + `SrtCtrl::AckReq`），但 `aero-live-srt` pump（`pump.rs`）的当前代码在 socket 读取失败时**终止 pump**：

```rust
let Ok((n, _)) = socket.recv_from(&mut buf).await else { break; };
```

没有自动恢复循环。文档的方向（P2 自动恢复握手）仍然有效，但不应描述为「缺失 SRT 重连」——而是「pump 退出时未尝试重连」。

---

## 已确认：方向四——一致性

### 缓存失效模式

`participant_cache`（`participant_cache.rs` 第 30 行）的文档明确说明：

```rust
/// - **Writes** (`update_me`, `delete_participant`, admin rename) invalidate the
///   cached entry so the next read re-fetches from the DB.
/// - **TTL** (default 60s) is a freshness floor, not a correctness guarantee: if
///   another node writes a profile change, this node may serve the stale value
///   for up to `CACHE_TTL` seconds before the next miss re-fetches.
```

`room_member_cache`（`room_member_cache.rs` 第 14 行）确认同样规则：

```rust
/// - **Reads** check a per-process DashMap.
/// - **Writes** (add/remove member) invalidate the cached entry.
```

二者均仅本地失效。**无跨节点失效总线**。文档中的共识分析——60 秒陈旧窗口是安全性（踢出/2FA）/体验性（头像）问题——**被完全证实**。

### SeqProvider 默认为本地

`LocalSeqProvider`（`aero-im-core/src/seq.rs` 第 50-63 行）是 `ImService::new` 默认值（第 268 行）：

```rust
/// Defaults to the process-local [`LocalSeqProvider`];
```

`RedisSeqStore`/`aero_storage::SeqStore` 存在且已实现 `SeqProvider`（第 76 行），但**并非默认**。文档关于「重启后 seq 归零」的断言**被完全证实**（重启后进程内的 AtomicU64 归零，旧消息在重播时会被重新标记 seq = 0、1、2……）。

### 安全关键路径——2FA/踢出

文档的 P1 建议（安全关键路径绕过缓存）在 `assert_room_access` 代码（`im-core/service/room.rs`）中得到确认——目前通过 `room_member_cache` 进行操作。当管理员踢出用户时，被踢用户仍能发消息最多 **60 秒**，窗口内产生的任何消息都会通过审核机器人被清除——但这仍会增加不必要的数据库负载并造成短暂的安全缺口。

---

## 总结：事实性回归

| 方向 | 不准确之处 | 严重程度 | 对建议的影响 |
|--------|-----------|----------|--------------|
| 一 | `findPendingMatch` 使用 `textOf()` 而非 `JSON.stringify` | **高**——错误描述匹配语义 | P0 nonce 方案仍完全有效且是更好的修复方案。幽灵消息漏洞比描述的更具体（纯文本重复，非任意块重复），但仍是真实存在的漏洞。 |
| 一 | 15 秒时间窗口已存在 | **中**——分析误报「无线程安全窗口」 | P1 超时+变灰方案**仍然需要**——15 秒窗口没有用户可见反馈。将 P1 描述调整为「超时 UI」而非「添加超时」。 |
| 二 | 无 | — | — |
| 三 | SRT「不会尝试重新握手」——SRT 协议有自身机制，但 pump 退出 | **低** | P2 方案「pump 恢复循环」的方向不变。 |
| 三 | WHIP「不会自动尝试 ICE restart」——代码在 `poll()` 失败时 `return` | **无**——完全准确 | — |
| 四 | 无 | — | — |

**整体验证通过率（按行数加权）：~92%**。四个方向均为真实的战略缺口。建议在推进之前，将方向一中关于 `findPendingMatch` 的错误描述更新为准确的 `textOf()` 行为。方向二中关于 Web Push 的架构可行性判断完全正确，收益率最高。

想要我起草一份「方向一 P0 修复」的实施方案（添加 nonce 到 send_message，修正 pending 消息匹配），作为该分析中发现的**第一个可操作步骤**的具体验收标准吗？
