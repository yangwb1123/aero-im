已验证。文档文件 `2026-07-11-second-round-global-scan-five-uncovered-directions.md`（38KB）已存在于 `docs/requirements/` 目录中。我已完成全代码库验证，生成了验证报告至同目录下的 `.out.md` 文件。

---

## 验证摘要

### 关键发现：**方向三的关键数字偏差 3 倍**

文档断言「AI Worker 默认 12 worker，每秒钟 12 次 DB 查询」，但代码显示：

| 指标 | 文档声称 | 实际代码 |
|------|---------|---------|
| 默认并发 | 12 | **4** (`WorkerConfig::default().max_concurrency`) |
| BATCH_SIZE | — | **8** (常量) |
| IDLE_SLEEP | — | **1s** (空闲轮询间隔) |
| Semaphore 并发上限 | 12 | **4** (`Semaphore::new(cfg.max_concurrency)`) |

### 其他偏差

| 位置 | 文档写 | 实际 |
|------|-------|------|
| JWT 过期 env 路径 | `AERO_JWT_EXPIRY_SECONDS` | `AERO__AUTH__ACCESS_TTL_SECS` |
| 配置文件默认 access_ttl | 900s/15min | 实际 config.toml 是 **3600s**（1h） |
| FileKind 变体 | `File, Code` | `Document, Other` |
| ServerFrame 数量 | 十六种 | **18 种**（漏计 Error + Pong） |
| 客户端 refresh 方法 | `api.refreshToken()` | 不存在（需直接用 fetch） |

### 优先级建议

| 方向 | 原优先级 | 验证后 | 原因 |
|------|---------|-------|------|
| 一：WS JWT | **P0** | **P0** | ✅ 确认——最直接影响用户的痛点 |
| 二：配置管理 | **P1** | **P1** | ✅ 确认——且发现 `jwt_additional_public_keys` 已被部分支持 |
| 三：资源争用 | **P1** | **P1→P2** ⚠️ | 核心论点成立但被 3× 夸大 |
| 四：多模态 AI | **P2** | **P2** | ✅ 确认 |
| 五：WS 测试 | **P2** | **P2** | ✅ 确认 |

验证报告详见于：`docs/requirements/2026-07-11-second-round-global-scan-five-uncovered-directions.out.md`
