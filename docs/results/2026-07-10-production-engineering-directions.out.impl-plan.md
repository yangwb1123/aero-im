我已完整阅读了第五轮生产工程化扫描文档。以下是Tech Lead分析报告。

---

# Tech Lead 分析报告：Aero IM 生产工程化扩展

> **分析对象**: `docs/requirements/2026-07-10-production-engineering-directions.md` — 第五轮全局扫描
> **日期**: 2026-07-12 | **角色**: Tech Lead
> **范围**: 5 个方向 × P0/P1/P2 共 21 个子任务

---

## 1. 任务分解

### 1.1 方向一：Web 安全硬化（P1·安全）

| ID | 标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|------|---------|------|------|---------|
| TASK-001 | 启用安全头：X-Frame-Options / X-Content-Type-Options / HSTS | `server/src/bin/boot/serve.rs:55-57` | 无 | 0.5h | 响应含 `X-Frame-Options: DENY`、`X-Content-Type-Options: nosniff`、`Strict-Transport-Security: max-age=31536000`；`securityheaders.com` 评分提升 |
| TASK-002 | 设置 CSP 默认基线策略 | `serve.rs:80-91`、`config.example.toml` | 无 | 1h | 默认发送 `Content-Security-Policy: default-src 'self'; ... frame-ancestors 'none'`；`AERO_CSP_POLICY` 可覆盖 |
| TASK-003 | 生产 CORS 启动阻断（空 origin 时 panic 而非 warn） | `serve.rs:221-227` | 无 | 0.5h | `cors_allowed_origins` 空且 `AERO_CORS_REQUIRE_ORIGINS` 设置时 panic 退出 |
| TASK-004 | 为 CDN 资源添加 SRI integrity 哈希 | `web/index.html` | 无 | 1h | 所有 CDN `<script>`/`<link>` 含 `integrity` 属性；CI 中 integrity-check 脚本验证 |
| TASK-005 | 集成 Mozilla Observatory CI 扫描 | CI + `scripts/security-scan.sh` | TASK-001,002 | 1h | 部署后自动评分，<B 级阻断 CI |
| TASK-006 | SAML 签名验证生产化决策 | `server/src/saml.rs:429-479`、Dockerfile | 无 | 4h | 决定 samael/bergshamra 路线；Docker 镜像含 `xmlsec1` 或文档明确说明 |

### 1.2 方向二：API 文档工程化（P2·DevEx）

| ID | 标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|------|---------|------|------|---------|
| TASK-007 | 引入 utoipa macro 自动生成 OpenAPI spec | `server/Cargo.toml` 、每个 handler 文件 | 无 | 12h | 所有公开 REST handler 标注 `#[utoipa::path]`；`/api/openapi.json` 覆盖率 >140 端点 |
| TASK-008 | 挂载 Swagger UI 交互式文档 | `server/src/routes/docs.rs` | TASK-007 | 1h | `/api/docs` 展示交互式 API 文档 |
| TASK-009 | API 版本前缀（`/api/v1` + `/api/v2`） | `server/src/routes.rs`、`routes/build.rs` | TASK-007 | 6h | 当前端点提供 `/api/v1/*` 别名；新功能走 `/api/v2/*` |
| TASK-010 | OpenAPI spec 变更检测 CI | `.github/workflows/ci.yml` + `scripts/openapi-diff.sh` | TASK-007 | 2h | PR 中自动 diff spec，标记 `BREAKING` 变更 |
| TASK-011 | WebSocket Event Catalog 文档化 | `common/src/model/ws.rs` + 文档 | TASK-007 | 4h | `WsClientFrame`/`ServerFrame` 生成独立 event catalog JSON |

### 1.3 方向三：tracing 宏覆盖与结构化日志（P2·Observability）

| ID | 标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|------|---------|------|------|---------|
| TASK-012 | 为所有 Axum handler 添加 `#[instrument]` | `server/src/` 所有 handler 文件 | 无 | 8h | 每个 handler 有命名 span；Jaeger 可按端点筛选 trace |
| TASK-013 | 为关键 service 方法添加 `#[instrument]` | `im-core/service/`、`ai/service/` | 无 | 4h | `send_message`、`edit_message`、`delete_message`、`answer_question` 等 ~30 方法有 instrument |
| TASK-014 | 制定结构化日志字段规范 + lint 规则 | `server/src/telemetry.rs`、`.clippy.toml` | 无 | 3h | 所有 `tracing::info!` 使用标准化字段；clippy lint 检测字符串插值 |
| TASK-015 | 添加 tracing 日志脱敏 subscriber layer | `server/src/telemetry.rs` | 无 | 2h | `password`/`token`/`secret`/`key` 字段自动替换为 `****` |
| TASK-016 | 提供生产日志配置文档 | `docs/operations/logging.md` | 无 | 1h | Loki/Promtail 集成示例、模块级别日志配置、轮转配置 |

### 1.4 方向四：依赖断路器与韧性（P2·Resilience）

| ID | 标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|------|---------|------|------|---------|
| TASK-017 | Redis 断路器：全局 `REDIS_DOWN` 标志 + 统一降级 | `server/src/redis_breaker.rs`、各 Redis 消费者 | 无 | 6h | Redis 断开时 presence 返回空、rate limit 放行、session 回退 JWT-only |
| TASK-018 | NATS 后备总线（pending_events 表 + flusher） | `bus/src/pending.rs`、PG 迁移 | 无 | 6h | NATS publish 失败写入 `pending_events`；后台 flusher 自动重发 |
| TASK-019 | 读缓存降级（stale-while-revalidate） | `storage/src/participant_cache.rs` | 无 | 2h | PG 故障时 `get_or_fetch` 返回已有缓存值而非 panic |
| TASK-020 | 统一依赖故障分类框架 | `common/src/dependency.rs` | TASK-017,018 | 4h | 定义 `DependencyKind::*` + 故障响应策略枚举；新模块接入强制声明 |

### 1.5 方向五：CI/CD 管线激活（P2·工程效能）

| ID | 标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|------|---------|------|------|---------|
| TASK-021 | 创建 Dockerfile 多阶段构建 | `Dockerfile`、`.dockerignore` | 无 | 2h | `docker build` 产出可运行容器；web 静态文件内嵌 |
| TASK-022 | 启用 CI 中 security-audit + coverage job | `.github/workflows/ci.yml` | 无 | 1h | `cargo deny check` 通过才绿；`cargo llvm-cov` 报告覆盖 >60% |
| TASK-023 | 创建 docker-compose.production.yml | `docker-compose.production.yml`、`docs/deployment.md` | TASK-021 | 3h | 生产级配置：非 root、只读 FS、cap-drop-all、健康检查 |
| TASK-024 | 集成测试 CI：PG + NATS + Redis service containers | `.github/workflows/ci.yml` | 无 | 4h | `#[ignore]` 集成测试在 CI 中自动执行 |
| TASK-025 | JS 前端单元测试框架 + 核心模块测试 | `web/` + CI step | 无 | 4h | `ws.js`、`api.js`、`context.js` 被 node:test 覆盖 |
| TASK-026 | CD workflow：Docker build → ghcr.io → staging deploy | `.github/workflows/cd.yml` | TASK-021 | 4h | push main 自动构建 → staging 部署 |
| TASK-027 | GitHub branch protection 配置 | 仓库设置 | TASK-022 | 0.5h | main 要求 CI 全绿 + 1 review + 线性历史 |

---

## 2. 执行顺序 & 依赖图

```mermaid
graph TD
    %% ===== P0 窗口（1-2天）=====
    subgraph P0["P0 窗口 · 1-2天"]
        T001[TASK-001: 安全头启用] --> T005[TASK-005: Observatory CI]
        T002[TASK-002: CSP 默认基线]
        T003[TASK-003: CORS 启动阻断]
        T022[TASK-022: CI security-audit + coverage]
    end

    %% ===== P1 窗口（2-3周）=====
    subgraph P1["P1 窗口 · 2-3周"]
        T017[TASK-017: Redis 断路器] --> T020[TASK-020: 依赖故障分类框架]
        T018[TASK-018: NATS 后备总线] --> T020
        T012[TASK-012: Handler #[instrument]] --> T013[TASK-013: Service #[instrument]]
        T012 --> T014[TASK-014: 日志字段规范]
        T021[TASK-021: Dockerfile] --> T023[TASK-023: docker-compose.prod]
        T021 --> T026[TASK-026: CD workflow]
    end

    %% ===== P2 窗口（3-6周）=====
    subgraph P2["P2 窗口 · 3-6周"]
        T007[TASK-007: utoipa 引入] --> T008[TASK-008: Swagger UI]
        T007 --> T010[TASK-010: OpenAPI diff CI]
        T007 --> T011[TASK-011: WS Event Catalog]
        T009[TASK-009: API 版本前缀] --> T010
        
        T014 --> T015[TASK-015: 日志脱敏]
        T014 --> T016[TASK-016: 日志配置文档]
        
        T019[TASK-019: 读缓存降级] --> T020
        
        T024[TASK-024: 集成测试 CI] --> T027[TASK-027: branch protection]
        T025[TASK-025: JS 测试框架]
        T022 --> T027
    end

    %% 并行任务组
    T004[TASK-004: SRI integrity] -.->|独立并行| T001
    T006[TASK-006: SAML 决策] -.->|独立并行| T001
```

### 可并行执行的任务组

| 组 | 任务 | 并行依据 | 所需资源 |
|----|------|---------|---------|
| **G1 — 安全头** | T001, T002, T003 | 都在`serve.rs`同一函数，一人一次性改完 | 1 人 × 2h |
| **G2 — CI 激活** | T022, T024 | CI YAML 不同 sections，可分开提交 | 1 人 × 5h |
| **G3 — 基础设施** | T021, T023, T026 | Docker 管线独立于 Rust 代码 | 1 人 × 9h |
| **G4 — 可观测性** | T012, T013, T014 | 可以分 crate 由不同人负责 | 2 人 × 8h |
| **G5 — 韧性** | T017, T018, T019 | Redis/NATS/PG 三者独立 | 2 人 × 7h |
| **G6 — API 文档** | T007, T009 | 主线不同（macro vs routes 重构） | 1 人 × 18h |

---

## 3. 技术风险

### 3.1 高风险项

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| **CSP `frame-ancestors 'none'` 破坏企业 iframe 集成** | 中 | 中 | CSP 作为默认基线，保留 `AERO_CSP_POLICY` env var 覆盖；文档注明企业集成场景 |
| **`utoipa` proc-macro 增加 3-5 min 编译时间** | 高 | 低 | CI 中配置 `actions/cache` 缓存 `target/`；考虑模块化编译（`cargo check -p aero-server` 仅检 server） |
| **Redis 断路器 `REDIS_DOWN` 全局 atomic 的并发竞争条件** | 低 | 高 | `AtomicBool` + `Ordering::Relaxed` 只做快速 gate；每个降级路径再检查实际连接；写穿透日志 |
| **NATS pending_events 表在长时间 NATS 故障时无限增长** | 中 | 中 | 设行上限 10,000 + TTL 24h + dead 标记 + 告警 |
| **SRI integrity 哈希在 CDN 库升级时静默失效 → JS 阻塞** | 高 | 高 | CI 中 integrity-check 脚本检测哈希变化 → 阻断 CI + 提示更新 |
| **HSTS `includeSubDomains` 对开发 `localhost` 产生影响** | 低 | 低 | 开发环境 config 默认无 HSTS；生产仅通过 `AERO__SERVER__FORCE_HTTPS` 启用 |

### 3.2 外部依赖

| 依赖 | 用途 | 替代方案 | 就绪度 |
|------|------|---------|--------|
| `cargo-deny` | 安全审计 | `cargo-audit` + `cargo-deny` | `deny.toml` 已存在 |
| `cargo-llvm-cov` | 代码覆盖 | 无（Rust 标准方案） | 需 CI 安装 |
| `utoipa` | OpenAPI 生成 | `aide`、`okapi` | 需评估编译影响 |
| `fred` (Redis) | 断路器 DisconnectHandler | 已有依赖，无需新增 | 现成 |
| `samael` / `bergshamra` | SAML XML-DSig 验证 | 已有依赖 | 需深度审计 |

### 3.3 性能瓶颈

| 场景 | 瓶颈 | 优化策略 |
|------|------|---------|
| `#[instrument]` 在高 QPS 端点的 span 创建开销 | ~200ns/span | `level = "debug"` + 生产 `EnvFilter` 只采 error/warn；采样率配置 |
| `utoipa` 编译期宏扩展 | ~2ms/handler | 仅公开 API handler 加 macro，内部函数免除 |
| CI 中 Docker 多阶段构建首次 | ~30min | `actions/cache` 缓存 `~/.cargo` 和 `target/` |
| `pending_events` 表 NATS 恢复后批量补发 | 瞬间流量尖峰 | flusher 限速（100 事件/秒）+ 指数退避重试 |

---

## 4. 资源评估

### 4.1 人员需求

| 角色 | 数量 | 技能要求 | 负责方向 |
|------|------|---------|---------|
| **Rust 全栈（安全）** | 1 人 | Axum 中间件、安全头配置、CSP 策略 | 方向一（T001-T006） |
| **Rust 后端（可观测性）** | 1 人 | `tracing` crate 深度经验、OTel、结构化日志 | 方向三（T012-T016） |
| **Rust 后端（韧性）** | 1 人 | Redis/NATS/PG 操作、断路器模式、fred 连接管理 | 方向四（T017-T020） |
| **Rust 后端（API 文档）** | 1 人 | utoipa/aide、Axum 路由治理、OpenAPI 规范 | 方向二（T007-T011） |
| **DevOps / CI** | 1 人 | GitHub Actions、Docker 多阶段构建、容器化部署 | 方向五（T021-T027） |
| **前端（JS 测试）** | 0.5 人 | node:test、ES2020 模块测试 | T025 |

**最优方案**: 2 人 full-stack Rust（一人各负责 2-3 方向）+ 1 人 DevOps（负责方向五全部 + 各方向 CI 集成），周期 6 周可完成全部 27 个任务。

### 4.2 里程碑时间线

| 里程碑 | 时间点 | 交付物 | 依赖 |
|-------|--------|--------|------|
| **M1: 安全基线** | Day 2 | T001+T002+T003+T022 完成 → 3 安全头 + CSP + CI 门禁 | G1+G2 |
| **M2: 容器化** | Day 5 | T021 Dockerfile + `docker build` 通过 | G3 单线 |
| **M3: 可观测性基础** | Day 10 | T012+T013 handler instrument 全覆盖 → Jaeger 可定位慢端点 | G4 |
| **M4: 韧性基础** | Day 12 | T017+T018 Redis 断路器 + NATS 后备 → 核心依赖故障时服务不垮 | G5 |
| **M5: API 文档** | Day 18 | T007+T008 OpenAPI 覆盖率 >90% + Swagger UI + diff CI | G6 |
| **M6: CD 就绪** | Day 22 | T023+T024+T026 docker-compose.prod + 集成测试 CI + CD 自动部署 | 全前序 |
| **M7: 完整发布** | Day 28 | T027 branch protection + 全部 CI 门禁 + 发布流程自动化 | M6 |

### 4.3 阻塞点（Blockers）

| 阻塞 | 描述 | 解决策略 | 应急方案 |
|------|------|---------|---------|
| **utoipa 编译兼容性** | proc-macro 可能在 workspace 中触发 MSRV 问题或与现有 derive macro 冲突 | 先在 `aero-server` 单 crate 验证；分步骤（先 GET handler，再 POST/write handler） | 回退到手动维护 OpenAPI JSON + `build.rs` 生成脚本 |
| **SAML 签名审计** | `samael` vs `bergshamra` 路线需要安全团队审计 | 文档中标记为 P2 季度级，当前不做阻塞 | 保持 `AERO_SAML_EXPERIMENTAL_VERIFY` 实验状态 |
| **CI runner 资源** | `cargo test -- --ignored` + `cargo llvm-cov` + Docker build 可能超 GitHub Actions 免费配额 | 自建 CI runner（`actions-runner` Docker 容器） | 分阶段 job：lib-test + integration-test 分开 workflow |
| **Redis 断路器 `DisconnectHandler` 的 fred API 兼容性** | fred 9 的 `DisconnectHandler` trait 签名可能变化 | 先读 `fred 9` 源码确认；写 `redis_breaker.rs` 单元测试覆盖 | 回退到轮询 `PING` 检测（~30s 延迟可接受） |

---

## 5. 质量保证

### 5.1 单元测试覆盖

| 任务 | 测试要求 | 覆盖率目标 | 测试类型 |
|------|---------|-----------|---------|
| TASK-001~003 安全头 | 验证响应 header 存在且值正确 | 100% | integration `axum::test` |
| TASK-004 SRI | integrity hash 匹配脚本（CI 验证） | N/A（CI 静态检查） | shell 脚本 |
| TASK-007 utoipa | 每个 handler 的 OpenAPI schema 输出正确 | >90% 端点 | `utoipa::OpenApi::spec()` 断言 |
| TASK-012~013 instrument | span 创建测试 | 100% handler | `tracing-test` crate |
| TASK-017 Redis 断路器 | `REDIS_DOWN` 时各路径行为 | 100% 降级路径 | mock Redis + `#[cfg(test)]` |
| TASK-018 NATS 后备 | 写入 pending_events + flusher 重发 | 95% 分支覆盖 | 事务仓储 mock |
| TASK-022 覆盖率门禁 | 全 workspace lib >60% | 逐季度提升 5% | `cargo llvm-cov` |

### 5.2 集成测试策略

| 测试场景 | 技术方案 | CI 位置 | 运行频率 |
|---------|---------|---------|---------|
| 安全头实际 HTTP 响应 | `axum::test` + `reqwest` | `cargo test -- --ignored` | 每次 push |
| OpenAPI spec 与实际路由一致性 | `utopia` + `walkdir` 扫描路由 | CI 独立 step | 每次 push |
| Redis 故障降级 | `testcontainers` + mock Redis server | CI `services: redis` | PR 时 |
| NATS 故障后备 | mock `EventBus` trait | 单元测试内 | 每次 push |
| Docker 构建 | `docker build --check` | CI `docker` job | PR 时 |
| CD 部署流程 | `act` 模拟 deploy step | CI 手动触发 | 发布前 |

### 5.3 代码审查要点

| 方向 | Code Review 红线 |
|------|-----------------|
| 安全头 | 不可引入 `frame-ancestors` 不兼容的 `X-Frame-Options` 冲突；HSTS `max-age` 生产必须 ≥31536000 |
| utoipa | 所有公开 handler 必须有 `#[utoipa::path]`，且 `responses` 覆盖所有 status code |
| instrument | `#[instrument(skip_all)]` 不可丢失 `fields(request_id)` 参数 |
| Redis 断路器 | 每个降级路径必须 emit `warn!` 日志，标明 `[REDIS_DOWN]` 前缀 |
| NATS 后备 | `pending_events` 必须有 `retry_count` 上限（5）+ `dead_letter_at` 时间戳 |
| Docker | 运行时用户必须为非 root（`USER 10001:10001`），文件系统 MUST 为 `read-only` |
| CI/CD | 不可在 CD workflow 中硬编码 production secrets——使用 GitHub Actions Secrets + OIDC |

### 5.4 性能测试需求

| 测试 | 场景 | 通过标准 |
|------|------|---------|
| 安全头延迟 | 启用所有安全头后的 HTTP 响应 p99 | <1ms 增加 |
| CSP 解析延迟 | 含 CSP 的 HTML 页面首次渲染 | <50ms 增加 |
| `#[instrument]` QPS 影响 | 1000 RPM handler 上启用 instrument | <3% 吞吐量下降 |
| Redis 断路器切换 | Redis 断开→降级路径响应时间 | <500ms 切换时间 |
| Docker 构建缓存效率 | 二次构建 vs 首次构建 | <5min vs <30min |
| NATS 后备补发吞吐 | flusher 从 pending 表重发 1000 事件 | <30s 完成 |

---

## 6. 实施计划

### 阶段 1（Day 1-2）：安全基线 + CI 门禁

```
Day 1  [T001启用安全头] [T002 CSP基线] [T003 CORS阻断] [T022 CI门禁]
Day 2  [T004 SRI] [T021 Dockerfile]
       ─── M1 安全基线完成 ───
```

**产出**:
- `serve.rs` 三个安全头取消注释 + CSP 默认策略
- CI 中 `security-audit` 和 `coverage` job 启用
- `web/index.html` 所有 CDN 资源含 `integrity`
- 首个 `Dockerfile` 多阶段构建

**风险切面**: 最小切割。即使其他方向延迟，安全基线 Day 2 即可上线。

### 阶段 2（Day 3-10）：基础设施 + 可观测性

```
Day 3-4  [T017 Redis断路器] [T018 NATS后备]
Day 5-6  [T012 Handler instrument] [T023 docker-compose.prod]
Day 7-8  [T013 Service instrument] [T014 日志规范]
Day 9-10 [T019 读缓存降级] [T020 依赖分类框架]
         ─── M2+M3+M4 完成 ───
```

**产出**:
- Redis 断开时 presence/rate-limit/session 三门降级
- NATS 故障时事件写入 `pending_events`，后台 flusher 自动重发
- 所有 150+ handler 带 `#[instrument]`，Jaeger 可按端点筛选 trace
- 结构化日志字段规范 + `clippy.toml` lint 规则
- `docker-compose.production.yml` 生产配置

**风险切面**: T017/T018 可并行（Redis vs NATS 不共享代码）。T012 可通过 sed 脚本批量操作（codemod），避免逐文件修改。

### 阶段 3（Day 11-18）：API 文档工程化

```
Day 11-12 [T007 utoipa引入 + 前50 handler]
Day 13-14 [T007 剩余100 handler + 模型derive]
Day 15-16 [T008 SwaggerUI] [T010 OpenAPI diff CI]
Day 17-18 [T009 API版本前缀] [T011 WS Event Catalog]
         ─── M5 API文档完成 ───
```

**产出**:
- OpenAPI spec 覆盖率从 1/150 → 140+/150
- `/api/docs` 交互式文档
- `/api/v1/*` 别名 + `/api/v2/*` 新路由树
- PR 中 OpenAPI diff 自动标注 breaking change

**风险切面**: T007 是最大工作量（12h）。建议分批：先 GET handler（~60 个，4h），再 POST/PATCH/DELETE（~90 个，8h）。T009 路由重构应与 T007 的分支隔离，避免冲突。

### 阶段 4（Day 19-28）：发布自动化 + 集成测试 + 收尾

```
Day 19-20 [T024 集成测试 CI]
Day 21-22 [T026 CD workflow] [T025 JS测试框架]
Day 23-24 [T005 Observatory CI] [T006 SAML决策] [T015 日志脱敏]
Day 25-26 [T016 日志文档] [T027 branch protection]
Day 27-28 稳定期 · 修复 CI flakiness · 文档补全
         ─── M6+M7 完整发布 ───
```

**产出**:
- CI 中集成测试（PG + NATS + Redis containers）
- CD pipeline（push main → Docker build → ghcr.io → staging deploy）
- `web/` 核心模块被 `node:test` 覆盖
- GitHub branch protection 配置完成
- 日志脱敏 + 日志聚合配置文档
- Mozilla Observatory B+ 评分

---

## 汇总统计

| 指标 | 值 |
|------|----|
| **总任务数** | 27 |
| **P0（1-2天可完成）** | 5（T001-T003, T004? T022） |
| **P1（2-3周）** | 10（T005,T012,T013,T014,T017,T018,T021,T023,T024,T026） |
| **P2（3-6周）** | 12（T006,T007,T008,T009,T010,T011,T015,T016,T019,T020,T025,T027） |
| **总预估工时** | ~72 人时 |
| **并行执行（2人）** | ~4 周 |
| **并行执行（3人）** | ~3 周 |
| **代码行数预估** | ~1,500（新代码）+ ~500（修改）+ ~200（配置/文档） |
| **外部 crate 新增** | 1-2（utoipa / aide） |

---

## 给团队的最终建议

1. **Day 1-2 先做 P0 安全头 + CSP**。零成本、高风险消除、无需设计评审——这是「有胜于无」的场景。做完即刻 PR，当天合并。

2. **Handler `#[instrument]` 别逐文件手动加**。写一个 `scripts/add-instrument.sh` sed 脚本：匹配 `async fn (handler.*)` 签名，在函数前插入 `#[instrument(skip_all, fields(request_id = %RequestId))]`。预计 30 分钟覆盖全部 handler。然后再花 4h 单独处理带 `State`/`Query`/`Path` 参数的复杂 handler。

3. **Redis 断路器和 NATS 后备是最有冲击力的 P1 任务**。这俩完成时，Aero IM 从「优美但脆弱的单机服务」变为「真正的分布式系统容错服务」。建议 Team Lead 本人或资深成员负责。

4. **utoipa 先做个 spike（2h 上限）**。在 `aero-server` 中引入 utoipa，给一个已有 handler（如 `GET /api/messages/{id}`）标注 macro，验证编译时间和 OpenAPI 输出正确性。如果编译时间增长 >15%，考虑 `aide` 作为备选。

5. **Dockerfile 不要最后做**。Day 2 就创建 `Dockerfile`，哪怕不完美。容器化后所有 CI 测试都可以在一致环境中运行，消除「it works on my machine」问题。

6. **不追求一次性完成全部 27 个任务**。P0 合并 → 剪彩 → P1 的第一个 PR → 后续分 sprint 迭代。方向四（断路器）和方向五（CI/CD）应该作为 sprint 1，方向二（API 文档）作为 sprint 2，方向三（日志）持续穿插。
