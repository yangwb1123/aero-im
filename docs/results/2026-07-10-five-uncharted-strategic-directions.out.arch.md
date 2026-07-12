# 架构分析报告：Aero IM 系统现状评估与演进路径

## 1. 架构评估

### 1.1 当前架构优势

Aero IM 呈现出**经过深思熟虑的事件驱动骨架**，有若干值得保留的设计选择：

- **进程内扇出隔离**：`Hub::fan_out_raw` 配合 bounded `mpsc`，将 NATS 的跨节点投递与单进程 WebSocket 扇出解耦。这是正确的层次切割——NATS 负责「至少一次可靠交付」，Hub 负责「局部高效广播」，两者失效模式不同。
- **crate 分层清晰**：从 `aero-common`（叶子）到 `aero-server`（组合）的单向依赖链，没有循环依赖。每个 crate 有明确的职责边界，`aero-common` 作为共享内核不含运行时依赖。
- **持久消费者 vs 临时消费者分离**：`im.room.*` 用 durable consumer（业务事件不能丢），`live.stream.*` 用 ephemeral consumer（弹幕可以丢）。这体现了**不同 QoS 需求**的正确建模。
- **预算控制器设计**：AiWorker 的 `CostBudget` + per-ws `KeyedCostBudget` + weighted all-or-nothing 是一套**有上限的公平调度**设计，防止单个工作区耗尽全局 AI 预算。
- **跨节点媒体最小化假设**：`CallBridge` 设计为「无中心控制面」的 UDP relay，仅在两端都在线时工作。不引入全局媒体控制层，降低了复杂度。

### 1.2 架构债务与技术债

根据验证报告发现的问题，我识别出以下几类债务：

| 债务类型 | 严重程度 | 具体问题 |
|---------|---------|---------|
| **安全债务** | 🔴 **P0** | `config.toml` 包含真实 RSA 私钥明文；TOTP secret 明文存储；无静态加密；无密钥管理 |
| **可观测债务** | 🟡 P1 | SFU 零 Prometheus 指标；带宽估计数据无法被运营消费；无出站字节核算 |
| **接口债务** | 🟡 P1 | 无批量 API；无标准导入/导出格式；`me_export.rs` 是功能性的但 Schema 未标准化 |
| **文档-代码漂移** | 🟢 P2 | SfuMediaSession 路径文档错误（实际在 `aero-server/src/`）；搜索 UI 存在但被标记为缺失 |
| **配置治理债务** | 🔴 P0 | 环境变量命名不一致（`AERO__` 双下划线 vs `AERO_RATE_LIMIT_` 单下划线双标准并存） |
| **测试覆盖债务** | 🟡 P1 | SFU/RTC 媒体路径仅有单元测试（`#[cfg(test)]`），缺少集成测试；CI 不跑真实媒体 E2E |
| **限流治理债务** | 🟢 P2 | 限流配置部分 fail-open（Redis 故障时放行），可能被利用 |

### 1.3 关键设计决策评估

**好的决策**：
- **事件总线作为跨实例事实源**（而非 gRPC 流或 Redis pub/sub）：NATS JetStream 提供持久化 + 至少一次语义 + 消费者组，天然支持水平扩展
- **集群状态集中在 Redis sorted-set**：避免了多节点内存状态不一致，`zadd` + `zremrangebyscore` 是天然的心跳过期机制
- **SFU 用进程内 HashMap 而非 Redis/外部存储**：媒体转发需要纳秒级决策，外部存储的延迟不可接受

**值得商榷的决策**：
- **`config.toml` 裸文件配置**：没有配置分层（默认值 → 环境覆盖 → 密钥服务）。当前用 figment 但 secret 仍然明文在文件系统
- **`data/` 目录由容器 root 创建**：这导致开发环境必须覆写 blob_dir/hls_dir，是容器化不彻底的表现
- **迁移编译期嵌入**：`sqlx::migrate!("../../migrations")` 将 migrations 烤入二进制，导致「必须先 build 再 migrate」的时序问题——这是 sqlx 的使用方式而非设计问题，但增加了部署复杂性

---

## 2. 扩展方向

### 方向 A：媒体可观测性 + 计费核算层 ⭐ ROIIII 高

**为什么需要**：
SFU 是系统中流量最大的子系统。当前有 `BandwidthEstimator`（拥塞控制）但无带宽核算（billable metric），无法回答「每个房间消耗了多少出站带宽」「谁的直播流成本最高」。这是从原型走向生产必须解决的缺失。

**核心挑战**：
1. **性能开销**：每个 RTP 包都核算会引入不可忽略的 CPU 开销。需要在精度与性能间权衡——是按包核算还是按时间窗口聚合采样？
2. **核算维度**：按（房间, 发布者, 订阅者, 时间）四维粒度还是更粗？过细则存储膨胀，过粗则无法做成本分摊
3. **热路径隔离**：核算不能阻塞 RTP 转发路径——必须是旁路写入环形缓冲区

**预期架构变更**：
- 在 `SfuForwarder::on_rtp` 旁增加 `ByteCounter` trait
- 引入 `aero-observability` crate（或复用 `aero-common`），定义 `MediaMeter` 接口
- 旁路写入 mmap 环形缓冲区 → 后台线程批量写入 Prometheus Histogram + 审计日志
- `BweKind` / `BandwidthEstimator` 数据导出到运营面板

**对现有系统的影响**：
- 低影响：不改变转发路径，只增加旁路记录
- `SfuForwarder` 是 `Arc<RwLock<SfuForwarder>>`，需要在写锁外采样避免锁竞争

### 方向 B：秘密管理层 ⭐ RIOII 紧急

**为什么需要**：
`config.toml` 包含 RSA 私钥、TOTP secrets 明文存储、无密钥轮换——这是**即时安全事件**。系统需要一个统一的秘密抽象层，使得不同秘密有不同生命周期。

**核心挑战**：
1. **现有消费方分散**：JWT 签名在 `aero-auth`，TOTP 在数据库，OIDC client_secret 在 config，S3 凭据在 env——每个消费方直接读不同来源
2. **密钥轮换需要版本化**：JWT 签名密钥轮换时，旧 token 在有效期内仍需验证，需要多版本密钥存储
3. **生产 vs 开发场景差异**：开发环境用文件系统，生产环境用 HashiCorp Vault/AWS Secrets Manager——接口需抽象

**选项分析**：

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. `SecretResolver` trait + vault crate** | 统一接口，生产级安全 | 引入外部依赖，开发环境需 mock |
| **B. 仅密封配置文件 + 环境变量** | 改动最小 | 不支持密钥轮换，审计困难 |
| **C. age/SOPS 加密 config 文件** | 无运行时依赖，git-safe | 密钥分发仍需手动 |

**建议**：先做**选项 C**（快速止血），同时在 `aero-common` 中定义 `SecretResolver` trait 作为**长期方向**。

**预期架构变更**：
- 新增 `aero-secrets` crate（或 `aero-common::secrets` 模块），定义 `SecretResolver` trait
- 实现：`EnvSecretResolver`（环境变量）、`FileSecretResolver`（加密文件）、`VaultSecretResolver`（HashiCorp Vault）
- 迁移：`config.toml` 中的敏感字段转为 `$SECRET_REF` 语法，由启动时解析
- 数据库 `totp_secrets.secret` 列加 `pgcrypto` 包装

### 方向 C：标准化导出/互操作层 ⭐ RIOIII 高

**为什么需要**：
GDPR 导出（`me_export.rs`）有功能性实现但无标准 Schema，批量迁移/备份不存在。这不仅是合规需求——如果用户无法从系统中导出数据，锁定的感知会阻碍企业采纳。同时，无标准导入意味着灾难恢复只能靠数据库 dump。

**核心挑战**：
1. **导出格式选择**：JSON 还是 NDJSON？单文件 vs zip 包？选择影响实现复杂度
2. **数据量级**：一个大工作区可能有百万级消息，需要流式导出而非全量加载到内存
3. **与 retention 策略的交互**：被 retention 清扫的消息是否导出？法务保全的数据导出时是否包含？

**建议决策**：NDJSON（每行一个 JSON 对象） + `.tar.gz` 打包。NDJSON 可流式生成、增量消费、无需完整 Schema 定义。

**预期架构变更**：
- 导出端点 `GET /api/workspaces/:id/export`（新增）vs 仅 `GET /api/me/export`（现有）
- 定义 `Exportable` trait：`fn export_stream(&self, ...) -> impl Stream<Item = ExportRecord>`
- `ExportRecord` 是 tagged enum（`Message`/`File`/`Reaction`/`Member`...）
- 实现方：`XRepo` 实现 `Exportable`（每仓储一个 `export` 方法）
- 批量备份：`aero-cli backup` 命令，走相同接口但输出到 blob store

**向后兼容**：新建端点/CLI 子命令，不改变现有行为。

### 方向 D：配置治理统一化 ⭐ RIOIII 高

**为什么需要**：
当前配置存在三种标准：`AERO__` 双下划线（figment 拆分）、`AERO_RATE_LIMIT_` 单下划线（plain env）、`AERO_S3_*` / `AERO_TRACE_SAMPLE_RATE` 等混合。新增配置项不确定用哪种风格，导致运维认知负担。另外 fail-open 策略不一致（Redis 故障时限流放行 vs DB 故障时 fail-closed）。

**核心挑战**：
1. **向后兼容**：已有部署用两种方式配置，修改解析规则会破坏现有部署
2. **figment 限制**：`Env::prefixed("AERO__").split("__")` 是当前选择，移出会导致配置重构

**建议**：
- 统一使用 `AERO__` 双下划线前缀，`AERO__RATE_LIMIT__PER_SEC` 替代 `AERO_RATE_LIMIT_PER_SEC`
- 在 `config.rs` 中**同时支持新旧变量名**（旧名映射到新名），标记 deprecated 日志警告
- 一个 release 后移除旧名支持
- 定义 `FailPolicy` 枚举：`FailOpen` / `FailClosed` / `FailDegraded`，每个外部依赖声明其故障行为

### 方向 E：测试基础设施现代化 ⭐⭐ RIOII 中

**为什么需要**：
媒体路径仅有 `#[cfg(test)]` 单元测试，缺少集成测试。真实 WebRTC 握手、WHIP 推流、SFU 转发无法在 CI 中验证。这是每次重构媒体代码时「手动冒烟」的风险源。

**核心挑战**：
1. **真实媒体需要浏览器/ffmpeg 对端**：纯 Rust 端到端测试需要启动 str0m peer + 发送实际 RTP 包
2. **时间依赖**：WebRTC ICE 握手需要秒级等待，测试时长增加
3. **非确定性**：网络抖动、协商顺序可能导致测试闪烁

**建议**：
- 在 `aero-live-webrtc/tests/` 中添加 `SfuIntegrationSuite`：启动内存 SfuRouter + 两个 str0m Peer 实例，验证选择性转发
- 使用 `trybuild` 风格而非 `#[tokio::test]` 的长超时测试
- 建立 `make test-integration` 目标，CI 中运行但不阻塞合并（flake gate）

---

## 3. 接口设计建议

### 3.1 新抽象层引入

**应该引入的三个核心抽象**：

#### 3.1.1 `SecretResolver` trait

```rust
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// 解析一个秘密引用（如 `$SECRET_REF:jwt/signing-key`）
    async fn resolve(&self, key: &SecretKey) -> Result<Vec<u8>, SecretError>;
    /// 列出所有已知密钥标识符（用于审计/轮换验证）
    async fn list_keys(&self) -> Result<Vec<SecretKey>>;
}
```

这个 trait 的引入使得 `aero-auth` 不再直接读 `config.keypair` 而是通过 resolver 获取当前版本 + 历史版本（用于验证旧 token）。所有消费方通过 `AppState.secrets` 访问秘密，而非直接读 config 字段。

**不引入这个抽象的风险**：每次新增一个秘密来源（S3、OIDC、FCM），都要修改 config 结构体。

#### 3.1.2 `ByteMeter` trait

```rust
pub trait ByteMeter: Send + Sync {
    fn record(&self, bytes: ByteCount, labels: &[Label]);
    fn snapshot(&self) -> Vec<MeterSnapshot>;
}
```

与 `SfuForwarder` 的 `on_rtp` 解耦。`on_rtp` 返回 `usize`（订阅者数量），而 `ByteMeter` 在旁路异步消费。`Label` 是 `(key, value)` 对，用于 Prometheus 维度标记。

**设计原则**：`ByteMeter` 的实现必须是 lock-free（`CrossbeamEpoch` / `Datadog` 风格的 sharded counter），避免在 RTP 热路径上引入锁。

#### 3.1.3 `Exportable` trait

```rust
#[async_trait]
pub trait Exportable {
    type Record: Serialize + Send;
    /// 流式导出该仓储的数据
    async fn export_stream(&self, scope: ExportScope) -> impl Stream<Item = Self::Record>;
}
```

每个 `XRepo` 实现此 trait。`ExportScope` 定义导出范围（时间窗口、频道、成员）。导出端点组合多个 `export_stream` 为单个 NDJSON 输出。

### 3.2 保持向后兼容的策略

对所有新增的 trait：
1. **默认实现**：`SecretResolver` 提供 `ConfigFileResolver`（读当前 config 的行为不变）
2. **特性门控**：`ByteMeter` 有 `NoopMeter` 实现（零开销），未配置时使用
3. **配置兼容层**：旧 env 变量通过 `config.rs` 中的 `deprecated_alias` 映射到新名

**多版本 API 策略**：
- REST API：使用 `Accept: application/vnd.aero.v1+json`（目前没有，不建议引入——收益不足以抵消复杂度）
- 替代方案：每次 breaking change 使用新路径 `/api/v2/...`，旧路径标记 deprecated 保留至少一个主版本

### 3.3 关键模块接口改进

**SFU 接口问题**：当前 `SfuForwarder::on_rtp` 返回 `usize`——这是一个信息丢失的反模式。调用方不知道转发到哪些 peer、丢包率、是否降级。建议改为：

```rust
pub struct ForwardResult {
    pub peer_count: usize,
    pub bytes_forwarded: u64,
    pub dropped_packets: u64,
    pub simulcast_layers: Vec<LayerId>,
}
```

这**不改变调用路径**（仍然是 `let _ = forwarder.on_rtp(...)` 可忽略返回值），但为上层提供挂载 meter 所需信息。

---

## 4. 技术选型

### 4.1 推荐的第三方依赖

| 需求 | 候选方案 | 推荐 | 理由 |
|------|---------|------|------|
| 秘密管理 | `hashicorp_vault` / `aws-secretsmanager` | `rust-vault` (Vault) | 通用性最好，自托管友好 |
| 配置加密 | `age` / `sodiumoxide` / `aws-kms` | `age` | 零运行时依赖，可与 git 配合 |
| 数据库加密 | `pgcrypto` / 应用层加密 | `pgcrypto` | 不走 Rust 内存，SQL 级透明 |
| 媒体指标 | `prometheus` crate / `opentelemetry` | 维持 `prometheus` | 已有 metrics 基础设施 |
| 配置解析 | `figment`（已有）/ `config-rs` | 维持 `figment` | 不引入竞争标准 |

### 4.2 评估新增依赖的标准

对于每个新依赖的引入，应通过以下检查清单：

1. **许可证兼容性**：MIT/Apache-2.0 优先，LGPL 可议（不静态链接），GPL/Apache 1.1 排除
2. **安全审计**：RustSec advisory 数据库无未修复 CVE
3. **MSRV 对齐**：依赖的 MSRV <= 当前 MSRV（1.80）
4. **unsafe 使用**：unsafe 代码必须可控且审计过
5. **升级风险**：依赖是否经历过 breaking change（参考 semver 合规历史）

### 4.3 自建 vs 采购决策

| 领域 | 建议 | 理由 |
|------|------|------|
| 秘密管理 | **自建 trait** + 对接现成后端 | 中间抽象层是竞争力的边界，具体后端可替换 |
| 媒体计费 | **自建** | 没有现成的 Rust SFU 计费框架；领域差异太大 |
| 配置加密 | **使用 age**（非自建） | age 是成熟标准，已审计，无理由重造 |
| 导出格式 | **使用 NDJSON**（不引入 Avro/Protobuf） | 导入导出需人类可读，NDJSON 是流式友好的适配标准 |

---

## 5. 实施路线图

### 优先级矩阵

```
                   紧急
                    │
        P0          │          P1
  秘密管理          │    媒体可观测性
  配置清理(config   │    计费核算层
  .toml RSA 私钥)  │    导出标准化
                    │
────────────┼──────────────
                    │
        P1          │          P2
  配置治理统一化    │    测试基础设施
  密钥轮换          │    移动推送优化 (已工作)
                    │    仅 aria 改进
                    │
                   不太紧急
```

### Phase 1：安全止血（1-2 周）

**目标**：消除已验证的安全事件

1. **P0** 从 `config.toml` 移除 RSA 私钥
   - 生成新的密钥对，私钥存入 `secrets/jwt_private.pem`（`-rw-------`）
   - `config.toml` 改为引用 `"file://secrets/jwt_private.pem"` 语法
   - 验证旧 JWT token 仍可验证（多公钥支持）

2. **P0** TOTP secret 加密
   - 迁移 `0044_totp_secrets.sql` 加 `pgcrypto` 包装
   - 现有记录在迁移脚本中加密（不使用应用层回调）

3. **P1** 配置变量命名规范对齐
   - 识别所有新旧风格环境变量
   - `config.rs` 添加 deprecated 映射层
   - 日志输出 deprecated 警告

### Phase 2：可观测性（3-4 周）

**目标**：SFU 和媒体路径可运营

1. **P1** `ByteMeter` trait 定义 + `PrometheusByteMeter` 实现
2. `SfuForwarder::on_rtp` 返回值扩展为 `ForwardResult`
3. BandwidthEstimator 数据导出（`layer_rates` HashMap → Prometheus Gauge）
4. 运营面板第一版（带宽成本、房间带宽排行、降级事件计数）

### Phase 3：互操作性（4-6 周）

**目标**：标准化导出 + 批量迁移能力

1. `Exportable` trait 定义
2. 每个 `XRepo` 实现 `export_stream`
3. `NDJSON` + `.tar.gz` 端点
4. `aero-cli` 增加 `export` / `backup` 子命令

### Phase 4：秘密管理层（持续）

**目标**：统一秘密抽象层

1. `SecretResolver` trait 定义
2. `ConfigFileResolver` 实现（当前行为不变）
3. `age` 加密 config 文件支持
4. 可选：Hashicorp Vault 接入

### 风险与缓解

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| 密钥轮换导致已登录用户被登出 | 中 | 高 | 多版本公钥验证，旧 token 在过期前有效 |
| 配置旧变量移除后破坏已有部署 | 低 | 高 | deprecation warnings 保留一个 release cycle |
| 媒体计费影响转发性能 | 中 | 中 | 采用采样核算 + 旁路写入，上线前压测 |
| 导出流内存消耗过高 | 中 | 低 | 流式 NDJSON + 数据库游标（cursor-based pagination） |
| 加密迁移破坏未加密 TOTP 备份 | 低 | 中 | 迁移脚本保留未加密回退路径，文档备注 |

---

## 总结

Aero IM 的核心架构骨架（事件驱动 + NATS 持久性 + 进程内扇出）是**正确的**。五个分析方向揭示的不是架构缺陷，而是**生产化不足**——系统从功能原型走向运营级产品的过程中，安全治理、可观测性、互操作性、配置管理四个维度处于滞后状态。

最紧急的干预点是 `config.toml` 中的明文 RSA 私钥和 TOTP 明文 secret——这是安全事件而不是设计选择。

最具长期价值的投资是媒体可观测性和计费核算层——SFU 是成本中心，不能衡量就无法管理。

三个新抽象层（`SecretResolver` / `ByteMeter` / `Exportable`）应该在引入时都有**零开销默认实现**，确保不因抽象而降低热路径性能。
