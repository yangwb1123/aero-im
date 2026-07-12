现在我已全面验证了所有代码证据。以下是评估：

---

## 验证评估：分析文档的代码证据核实

我已对 5 个方向逐一扫描了相关源码，以下是代码级验证结果：

### 方向一：DOM 无上限增长 ✅ 准确

**代码证据**：
- `app.js:789-791` — `appendMessageEl` 无条件调用 `els.msgList.appendChild(node)`，无回收逻辑
- `app.js:217,225` — 增量追加新消息 DOM
- `app.js:793-796` — `scrollToBottom` 使用 `requestAnimationFrame` 仅优化滚动位置，不节流 DOM 插入
- `app.js:619-621` — `rerenderCurrentRoom()` 的 `replaceChildren()` 仅在房间切换时调用，单会话内永不 pruning
- `render.js` 中每消息创建 ~20-40 个 DOM 节点（`renderMessage` + `buildBlocks` + 各种格式化）
- 零 `IntersectionObserver`、零虚拟滚动、零 `MAX_VISIBLE_MSGS` 守卫

**补充发现**：`app.js:551` 的 `els.msgList.replaceChildren()` + `app.js:552` 的 `els.msgEmpty.hidden = true` 是在切换时初始化，不是 DOM 回收。主循环（`onMessage` → `appendMessageEl`）一次性 append。

### 方向二：WS 上行无确认 ✅ 准确

**调用点验证**（`app.js` 中所有 ws.send*() 调用）：

| 行号 | 调用 | 检查返回值？ |
|------|------|-------------|
| 893 | `ws.sendMarkdown(roomId, md, replyTo)` | ❌ |
| 906 | `ws.sendMessage(roomId, blocks, replyTo)` | ❌ |
| 907 | `ws.typing(roomId, false)` | ❌ |
| 894 | `ws.typing(roomId, false)` | ❌ |
| 781 | `ws.editMessage(m.id, ...)` | ❌ |
| 827 | `ws.markRead(...)` | ❌ |
| 861 | `ws.typing(roomId, true)` | ❌ |
| 865 | `ws.typing(roomId, false)` | ❌ |
| 657 | `ws.react(mid, emoji)` (反应 chip) | ❌ |
| 678 | `ws.react(m.id, emoji)` (emoji picker) | ❌ |
| 681 | `ws.deleteMessage(m.id)` | ❌ |
| 101 | `ws.watchStream(sid)` (重连) | ❌ |

所有 12+ 调用处全部忽略返回值。`ws.js:184-191` 的 `send()` 正确返回 `boolean` 并在 WebSocket 非 OPEN 时返回 `false`，但调用方从不检查。

**乐观渲染路径**（`app.js:937-944`）创建了乐观 UI 条目（`pendingByTempId`），但**仅当 `ws.sendMessage`/`sendMarkdown` 被实际调用前才生效**——如果 `send()` 返回 `false` （连接断开会静默失败），乐观条目永远留在 DOM 上且无失败指示。

### 方向三：merge_hits 非 RRF ✅ 准确（附带一个重要发现）

**`merge_hits`（`routes/helpers.rs:22-45`）**：确实是取最大值合并。
```rust
// 第 29 行 — 选最大分数，而非 RRF
Some(existing) if existing.score >= h.score => {}
_ => { best.insert(id, h); }
```

**重要发现**：代码库中已存在正确的 RRF 实现——`aero-ai/src/rerank.rs` 的 `fuse_rankings`，带完整的单测（重叠取胜、空列表降级、ULID 时间戳决定 tiebreak）。但它只被 `AiService` 在 RAG 摘要/问答管线中使用（`service_impl.rs:262,291`），**不被搜索路由使用**。

搜索路由 `routes.rs:1579` 在 `mode=hybrid` 时调用 `merge_hits(fts, vec_hits, limit)`——因此文档中"RRF 标注与实际不符"的论点是准确的。**修复比文档建议的更简单**：直接替换为 `fuse_rankings` 即可，该函数已可通过 `aero_ai::rerank::fuse_rankings` 调用。

### 方向四：CI/部署/运维 ✅ 准确

- `.github/workflows/ci.yml:1-2` — 明确注释"当前无 CI runner，此配置作为就位准备"
- 7 个 job（check/test/size/truth/web/dependency/security）全部就绪但被注释阻断
- `docker-compose.yml` — 所有服务直接暴露端口（5432/6379/4222/9000），硬编码密码（`aero_dev_pw`/`aero_minio_pw`），纯 dev 网络（`aero-dev` bridge），无 TLS，无 TURN，无备份

### 方向五：P2P ICE 无 TURN ✅ 准确

`web/calls.js:74-76`：
```javascript
const cfg = state.rtcConfig
  ? { iceServers: state.rtcConfig.ice_servers || state.rtcConfig.iceServers }
  : { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] };
```

- 仅 Google 公共 STUN 作为 fallback，零 TURN URL
- 服务端 `aero-signaling` crate 已定义 `RtcConfig`/`IceServer` 类型，`GET /api/rtc-config` 端点已存在且在 `app.js:58` 调用——但返回的配置中不含 TURN relay
- `docker-compose.yml` 无 coturn 容器；`aero-server` 的 `services.rs`（`bin/boot/`）中没有 TURN 凭证逻辑

---

## 关键修正与补充建议

### 1. 方向三：存在现成的 RRF 实现，修复更简单

分析文档正确地指出了最大分数问题，但**修复比建议的更容易**。现有的 `aero_ai::rerank::fuse_rankings` 可直接在搜索路由中替换 `merge_hits`。只需 `routes.rs` 将：
```rust
fts = merge_hits(fts, vec_hits, limit);
```
替换为：
```rust
fts = aero_ai::rerank::fuse_rankings(&fts, &vec_hits, limit as usize);
```
并添加 `use aero_ai::rerank::fuse_rankings`。保留 `merge_hits` 作为 `"hybrid_max"` 备选 mode。改约 3 行代码 + 测试，< 1 小时。

### 2. 方向一：建议立即添加"粗糙防崩溃"的快速修复

完整的虚拟滚动预计 2-3 天，但可以在 `appendMessageEl` 中加一个**单行 DOM 回收守卫**立即阻止内存暴涨：

```javascript
function appendMessageEl(node, { scroll = true } = {}) {
  els.msgList.appendChild(node);
  while (els.msgList.childElementCount > 500) {
    els.msgList.removeChild(els.msgList.firstChild);
  }
  if (scroll) scrollToBottom();
}
```

不完美但能立竿见影阻止 OOM，后续再补虚拟滚动。

### 3. 方向二：出站帧队列已在设计上可快速构建

由于 `WsClient` 已经有 `_reconnectTimer`、`_lastSeen` 光标、`SeqGate` 等优雅重连基础设施，出站帧队列可以直接复用这些机制。建议：
- 在 `WsClient` 中加 `_pendingOutbound: Frame[]`
- 在 `_scheduleReconnect`/`_open` 中重放
- `send()` 返回 `false` 时自动入队并触发重连

### 4. 方向四：最小成本 CI 激活

所有 job 都已定义，只需移除 `ci.yml` 顶部的注释头和"注意"块。GitHub Actions 无需自建 runner——利用 ubuntu-latest 即可运行除 `integration-test`（需 PG/Redis/NATS services）之外的全部 6 个 job。

### 5. 方向五与现有基础设施的关系

分析文档未提及的一个细微之处：`aero-signaling` crate 的 `RtcConfig` 已经包含可选的 `username`/`credential` 字段（TURN 鉴权所需）。因此添加 coturn 不需要修改 API schema——只需：
1. `docker-compose.yml` 加 coturn 容器
2. `aero-server` boot 时填充 `RtcConfig.ice_servers` 中的 TURN URLs + 短期 HMAC 凭证
3. 前端 `calls.js` 代码不需要改（已从 `state.rtcConfig.ice_servers` 读取）

---

## 综合评估

| 维度 | 评价 |
|------|------|
| 代码证据完整性 | ★★★★★ — 每个方向都有精确到行号的引用 |
| 影响评估准确性 | ★★★★★ — 退化模型（方向一的 DOM 增长曲线）和故障链（方向二的静默丢消息）都紧扣代码 |
| 修复建议务实性 | ★★★★☆ — 方向一/二/三的建议可行；方向四的 CI 激活可以加速（只用 GitHub hosted runner 无需自建）；方向五未提现有 `aero-signaling/RtcConfig` 的 username/credential 字段已就绪 |
| 唯一疏漏 | 方向三未发现现有 `fuse_rankings` 实现（在 `aero-ai/src/rerank.rs`），使得修复看起来比实际更重 |
| 交叉验证 | 与 `docs/requirements/` 下 110+ 份现有文档无矛盾；方向二的 `optimisticAdd` 已在代码中（`app.js:937`），但分析正确地指出了它不解决 send() 本身失败的问题 |

**结论**：这份分析文档**质量很高**，5 个方向全部有充分且准确的代码证据支持。最大修正是方向三——修复比建议的简单得多，因为 RRF 实现已经存在。建议保留文档中 `helpers.rs` 用 max-score 的代码证据，但将修复方案从"新写 RRF"改为"直接使用现有 `fuse_rankings`"。
