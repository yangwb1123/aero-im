# Skill: Clean Architecture for Aero IM

**用途**：保持 crate 间的依赖方向正确，防止产生循环依赖或向上依赖。

---

## Aero IM 的依赖规则

```
crate 层次（自底向上）:

aero-common          ← 叶子，无内部依赖
aero-bus             ← 仅依赖 aero-common
aero-storage         ← 仅依赖 aero-common
aero-auth            ← 仅依赖 aero-common + aero-storage
aero-signaling       ← 仅依赖 aero-common
aero-im-core         ← aero-common + aero-bus + aero-storage + aero-signaling
aero-im-call         ← aero-common + aero-bus + aero-storage + aero-signaling
aero-ai              ← aero-common + aero-storage
aero-live-core       ← 仅依赖 aero-common
aero-live-rtmp       ← aero-common + aero-live-core
aero-live-hls        ← aero-common + aero-live-core
aero-live-whip       ← aero-common + aero-live-core + aero-live-hls
aero-live-webrtc     ← aero-common + aero-live-core + aero-live-hls
aero-live-srt        ← aero-common + aero-live-core + aero-live-hls
aero-push            ← 仅依赖 aero-common
aero-server          ← 依赖所有 crate（组合根）
```

**禁止**：
- `aero-common` 依赖任何其他 crate
- `aero-bus` 依赖 `aero-storage` 或 `aero-server`
- `aero-storage` 依赖 `aero-server` 或 `aero-im-core`
- `aero-live-core` 依赖 `aero-live-rtmp`/`-hls`/`-whip`/`-webrtc`/`-srt`
- 任何 crate 依赖 `aero-server`

### 依赖违反检查

```bash
# 快速检查 aero-common 没有非法依赖
grep -E "aero-(bus|storage|auth|im|live|push|server)" crates/aero-common/Cargo.toml && echo "❌ 违反!" || echo "✓ OK"

# 检查 aero-storage 没有依赖 aero-server
grep "aero-server" crates/aero-storage/Cargo.toml && echo "❌ 违反!" || echo "✓ OK"

# 检查 aero-bus 没有依赖 aero-storage
grep "aero-storage" crates/aero-bus/Cargo.toml && echo "❌ 违反!" || echo "✓ OK"
```

## 新增功能的放置决策树

新增功能属于哪个 crate？

```
功能是纯数据模型/ID/类型/配置?
  ├── 是 → aero-common
  └── 否
功能是 NATS 总线通信?
  ├── 是 → aero-bus
  └── 否
功能是数据库/Redis 交互?
  ├── 是 → aero-storage (+ 对应迁移)
  └── 否
功能是认证/JWT/OIDC?
  ├── 是 → aero-auth
  └── 否
功能是 IM 业务逻辑（消息/房间/反应/通知）?
  ├── 是 → aero-im-core
  └── 否
功能是通话编排/roster?
  ├── 是 → aero-im-call
  └── 否
功能是 AI/嵌入/LLM调用?
  ├── 是 → aero-ai
  └── 否
功能是直播摄入/转码/推流?
  ├── 是 → aero-live-core / -rtmp / -hls / -whip / -webrtc / -srt
  └── 否
功能是推送网关?
  ├── 是 → aero-push
  └── 否
功能是 HTTP/WS 路由、bot、Hub?
  └── 是 → aero-server
```

## 警告信号

- `aero-storage` 中出现 HTTP 相关导入（`axum`, `http`）→ 错误，应移入 `aero-server`
- `aero-im-core` 中出现 `tokio::spawn` 或 `CancellationToken` → 警告，启动逻辑应归 `aero-server`
- `aero-common` 中出现 `sqlx` 或 `fred` → 严重错误，common 是叶子
- `aero-bus` 中出现业务逻辑（判断房间类型、检查权限）→ 错误，bus 只处理投递
- 任何 crate 中的 `use aero_server::...` → 严重依赖违反
