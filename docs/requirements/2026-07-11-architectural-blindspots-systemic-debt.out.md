现在我已经完成了全面的代码验证。以下是每项主张的逐项验证结果：

---

## 验证结果：5 项主张中有 4 项经代码确认，1 项存在夸大的表述

### 方向一（P0）：后台任务健康治理
**裁决：✓ 已确认，严重程度甚至略高于描述**

- `background.rs` 中确实验证了 **23 个 `tracker.spawn()` 调用**，错误处理模式完全一致（`if let Err(e) = ... { tracing::error!(...) }`）
- 没有重启、没有心跳、没有健康信号
- **关键发现**：已审核的 7 个 bot（`agent_bot`、`ooo_bot`、`push_bot`、`golive_bot`、`transcribe_bot`、`moderation_bot`、`unfurl_bot`）均 **不接收 `CancellationToken`**。它们无法优雅关闭，也无法被监控框架重新 spawn。它们运行，或者崩溃。这就是全部。
- `/health/ready`（`routes/health.rs`）仅探测 PG/Redis/NATS/blob —— 不探测后台任务
- 启动时没有从总线监听器发出信号（`run_bus_listener`、`run_live_bus_listener` 也不接收 token）
- "客户端连接成功但收不到事件"的攻击面是真实的：NATS `subscribe()` 在 `run_bus_listener` 内部是异步的，而 HTTP listener 在 `spawn_background` 返回后立即绑定

### 方向二（P1）：前端测试真空
**裁决：✓ 已确认，但论据略有夸张**

| 文件 | 行数 |
|------|------|
| web/*.js（不含 eslint） | ~4318 |
| 测试文件 | **0** |

实际的 JS 规模约为 **~4300 行**（而非文档中提到的 ~2000 行），使得差距更大。

`.catch()` 模式确实很普遍：
- **`web/app.js`**：6 个 `.catch(() => {})`，2 个用于 API 调用（`rtcConfig`、`liveGifts`、`listReceipts`、`reactionsBatch`），1 个用于 `Notification.requestPermission`——全部静默吞掉错误
- **`web/render.js`**：2 个 `.catch()` 适当地重新启用 UI 元素。不是静默的
- **`web/api.js`**：1 个 `.catch(() => null)` 用于 JSON 解析——很好
- **`web/calls.js`**：3 个 `.catch()` 记录到 `console.warn`——合理

"95% 的 API 调用没有错误处理"是过度夸张的辩护。在 7 个 API 调用中，3 个有静默的 `.catch(() => {})`，2 个有 UI 恢复，1 个返回 null，1 个记录日志。大约是 **43% 静默、57% 有某种处理**——但这仍然意味着几乎一半的 API 故障在用户面前完全无声无息。

### 方向三（P0）：配置管理混乱
**裁决：✓ 已确认，实至名归的 P0**

- 在 `crates/aero-server/src/bin/boot/` 中 **已验证超过 45 个 `env::var()` 调用点**
- 命名约定混合得到确认：同一文件（`background.rs` L79 vs L228）中，`AERO__`（figment 风格）和 `AERO_`（普通风格）并存
- 启动时验证为零——如果缺少 `AERO_UNFURL`，unfurl_bot 直接不启动，**没有警告**；如果是拼写错误，功能默认静默降级
- 没有 `/debug/config` 端点——运维人员无法检查运行时生效的配置
- `retention.rs` 单独包含 13 个 `env::var()` 调用，每个都有 `unwrap_or()` 默认值
- 没有集中化的配置模式——每个模块都有自己的包级 "parse-or-default" 样板代码

### 方向四（P1）：启动编排
**裁决：✓ 已确认**

- `main.rs` 的启动顺序确实是线性的，完全如所述（13 个步骤，从 config → persistence → repos → services → ingest → orchestration → state → background → metrics → retention → serve）
- 第 10 步（`spawn_background`）返回后，第 13 步（`serve`）立即开始——**23 个后台任务可能仍在初始化**
- 没有启动就绪信号——`run_bus_listener` 在其 NATS 消费者注册完成时不会通知
- "at-least-once 语义的启动窗口漏洞"是真实的：在 `run_bus_listener` 订阅 NATS subject 之前发布的 `RoomEvent` 会被错过
- 平滑降级没有被信号通知——如果 Redis 宕机，presence 静默降级，没有指标或端点指示

### 方向五（P2）：灾难恢复
**裁决：✓ 已确认，甚至比所述更糟**

- **DR runbook 目录仅包含 1 个文件**：`docs/runbooks/messages-partitioning.md`（一个迁移操作指南）。没有 `disaster-recovery.md`
- 没有 `scripts/db-backup.sh`，没有 `scripts/db-restore.sh`
- 数据生命周期被分散到 13 个独立的 `AERO__SERVER__*_RETENTION_DAYS` 环境变量中，没有总体留存矩阵文档
- `docs/requirements/` 下有 **110+ 份分析文档**，但 **零份运营 runbook**
- 合规导出（GDPR 数据主体访问请求）没有自动化端点——只有 `GET /api/me/export`（仅单个用户，无管理批量导出）
- 跨存储一致性在备份/恢复场景中完全没有被解决

---

## 附加发现：超出原始范围

在验证过程中发现的、文档未提及的应值得注意的问题：

1. **缺少 CancellationToken 的普遍性**：7 个 bot 中除了有 `ai_shutdown` 的，**没有**任何通过 `CancellationToken` 优雅关闭。`run_bus_listener` 和 `run_live_bus_listener` 也忽略了 token。它们无法被外部关闭，这意味着 Docker 停止超时和 Kubernetes pod 驱逐可能导致数据丢失。

2. **`docs/requirements/` 蔓延**：该目录下有 **110+ 份分析文档**，许多是重复迭代（例如，有 6 份日期为 2026-07-09 的文件命名为 `*-five-*-gaps*.md`）。这是元层面的技术债务——分析疲劳消耗了应该修复债务的认知带宽。

3. **估算对比**：
   - Rust 代码：`find . -name "*.rs" -not -path "*/target/*" | xargs wc -l | tail -1` → 接近 **~46K** ✓
   - Routes.rs：**2854 行** ✓（分析声称 2854）
   - 迁移：**157** ✓

---

## 综合评估

文档定位准确。五个方向中有四个已被代码验证为真实问题，前端错误处理说法虽略有夸张，但基本面成立。

**最紧迫的问题**是后台任务健康治理（P0）和配置管理混乱（P0）——它们共同意味着系统可以静默地、部分地失败，而没有操作员可见性。启动编排（P1）通过一个简单的启动就绪信号通道模式就可以取得很大进展。前端测试（P1）将从纯函数测试（`SeqGate`、`api.js` 的核心调用）中受益，而不需要 DOM 模拟。灾难恢复（P2）在基础设施投入运营之前可以暂缓，但 runbook 是脆弱的零成本胜利。
