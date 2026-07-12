Tech Lead 分析完成。文档已写入 **`docs/analysis/2026-07-12-tech-lead-implementation-plan-five-security-operational-directions.md`**。

---

## 实施计划摘要

### 5 个方向、35 个任务、135 工时

| 方向 | 优先级 | 任务 | 工时 | 关键交付物 |
|------|--------|------|------|-----------|
| 一、Token & 会话安全 | **P0** | 8 | 32h | JWKS 端点 + 密钥轮换 CLI + 设备绑定 + 并发上限 + MFA 步升 + PAT 2.0 |
| 二、依赖健康与启动验证 | **P0** | 7 | 23h | CircuitBreaker + 启动校验 + task 健康端点 + 降级层级 + NATS 退避 |
| 三、统一通知通道 | P1 | 7 | 30h | 通道抽象 + 邮件队列/HTML + Web Push VAPID + 偏好路由 + 缺失通知源补齐 |
| 四、直播生产化 | P1 | 7 | 25h | 直播 metrics + HLS 段延迟 SLI + 截帧审核 + ABR 多码率 + Jaeger 链路 |
| 五、外部 API 平台 | P2 | 6 | 25h | API Key 2.0 + 版本中间件 + OpenAPI 自动生成 + Webhook Schema 版本 + 开发者门户 |

### 核心设计决策

1. **方向一利用现有 `verify_keys: HashMap<String, DecodingKey>`**（`jwt.rs:45`），JWKS 轮换成本从"重构"降为"扩展"——多密钥结构已就位
2. **方向二手写 CircuitBreaker（~80 行）** 而非引入 `failsafe` crate，避免额外依赖。状态机：Closed→Open（5 次失败）→HalfOpen（10s）→Closed（1 次成功）
3. **方向三邮件队列化**（`tokio::mpsc`）将热路径延迟从 500ms→1μs，满缓冲则 log-skip 不阻塞
4. **方向四截帧审核复用既有 AI API 管线**（`aero-ai`），无 key 时静默降级（`ModerationResult::Skipped`）
5. **方向五 API Key 2.0 复用方向一 PAT 2.0 的 scope 体系**——统一 `messages:read`/`messages:write` 命名空间

### 推荐执行顺序

- **Week 1-2（并行快赢）**：方向一 Phase 1 (JWKS+设备绑定+并发上限) + 方向二全量 (断路器+启动校验) + 方向三 Phase 1 (通道抽象+邮件 Web Push)
- **Week 3-4（平台功能）**：方向一 Phase 2 (PAT 2.0+MFA) + 方向三 Phase 2 (全通知源覆盖) + 方向四全部 (直播加固)
- **Week 5-6（生态构建）**：方向五全部 (API 平台+开发者门户) + 全量集成测试

### 关键风险

- **R2 设备绑定锁合法用户**→ 宽松模式（告警邮件 + 二次验证，3 次不匹配才拦截）
- **R3 熔断器误触发**→ 滑动窗口 5 次连续失败（非 5 秒内），避免 autovacuum 毛刺
- **R6 截帧审核算力**→ 间隔可配置 + 本地 NSFW 降级避过 AI API
