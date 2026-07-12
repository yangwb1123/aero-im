所有证据收集完毕。以下是对文档 5 个方向的**全面验证报告**。

---

# 验证报告：5 方向战略分析文档的交叉验证

## 格式层发现

| 项目 | 文档声称 | 实际 | 判定 |
|------|---------|------|------|
| `docs/requirements/` 文档数 | 61 | **230**（全部 `.md`） | ❌ **数据偏差** |
| SFU 文件路径 | `aero-live-webrtc/src/sfu_forwarder.rs` | **不存在**；实际是 `aero-live-webrtc/src/forward/mod.rs` | ❌ **路径错误** |
| `SfuMediaSession` 路径 | `aero-live-webrtc/src/sfu_media.rs` | **不存在**；实际是 `aero-server/src/sfu_media.rs` | ❌ **路径错误** |
| `CallBridgeSupervisor` 路径 | 仅提及模块名 | 实际：`aero-server/src/call_bridge_supervisor.rs` | ✅ 虽未给路径，模块存在 |
| `me_export.rs` 引用 | 文档提及 | 实际存在 `aero-server/src/me_export.rs` | ✅ |
| TOTP 表名 | `totp_secrets`（文档）、`li_secrets`（文档代码注释） | 迁移文件 `0044_totp_secrets.sql`，代码仓库正确 | ⚠️ 语义误差 |
| `jwt_secret = "change-me"` | 第 1-10 行 | 实际使用 RS256 PEM 密钥，不是 HMAC secret | ❌ **配置结构描述错误** |

## 各方向验证

### 方向一：SFU 媒体成本模型 ⭐⭐⭐⭐

**核心声明验证**：

| 声明 | 验证 | 证据 |
|------|------|------|
| 无带宽核算 | ✅ **正确** | `forward/mod.rs` 的 `on_rtp` 返回转发计数（`usize`），但无法出站字节、无法计费。`on_bytes` 仅用于 `ThroughputEwma` simulcast 自适应 |
| 无 Prometheus 指标 | ✅ **正确** | `aero-live-webrtc/src/` 零 Prometheus 引用 |
| 无成本模型 | ✅ **正确** | 零 `cost`/`budget` 引用 |
| Simulcast 实现完备 | ✅ **有但无自适应** | `simulcast.rs` 有 `LayerSelector`、关键帧边界切换、冷启动——但自适应由 `BandwidthEstimator` + `LayerSwitchPolicy`（`bwe.rs`）驱动，基于订阅者反馈 |
| `BweKind::Narrow` | ✅ 外部 str0m API | str0m 0.19.0 有 `bwe::BweKind`，未直接引用 |

**额外发现**：
- `SfuForwarder` 真实架构比文档描述的更完善——有 `PublisherRembAggregator`（反向 REMB 给发布者）、`BandwidthEstimator`（AIMD + REMB + TWCC 融合）、`KeyframeGate`（聚合关键帧请求）。文档低估了 BWE 基础设施
- `throughput` 追踪按 `(pub_mid, rid)` 键值存入 `layer_rates`（`HashMap<(String, Rid), ThroughputEwma>`）——有粒度但只能被 `LayerSwitchPolicy` 消费，无导出

**纠正**：文档描述的 SFU 架构不完整——现有系统有**带宽估计**（用于拥塞控制）但缺少**带宽计费/核算**。这两个是不同的东西。

### 方向二：数据保护合规 ⭐⭐⭐⭐⭐

| 声明 | 验证 | 证据 |
|------|------|------|
| 无 GDPR/SOC2/CCPA 映射 | ✅ **正确** | 零匹配 |
| 无静态加密 | ✅ **正确** | 零 `encrypt`/`aes`/`pgcrypto` 引用 |
| 无密钥管理/KMS | ✅ **正确** | 零 `kms`/`vault`/`secret.*store` 引用 |
| TOTP 明文存储 | ✅ **正确** | `totp_secrets.secret` 列 `text NOT NULL` |
| PAT 哈希存储 | ✅ 文档确认正确 | `aero_pat_*` 使用 bcrypt |
| 附件未加密 | ✅ **正确** | `BlobStore` 接口无加密包装器 |

**额外发现**：
- `config.toml` 包含真实 RSA 私钥（非占位符）——比文档描述的 `"change-me"` 更严重。这意味着 **生产级别的 config 文件中含有可用的 JWT 签名密钥**
- 存在 `secrets/` 目录（`jwt_private.pem`/`jwt_public.pem`），权限是 `-rw-------`——至少文件权限正确

### 方向三：数据互操作与迁移 ⭐⭐⭐⭐⭐

| 声明 | 验证 | 证据 |
|------|------|------|
| 无 Slack/Mattermost/Teams 导入 | ✅ **正确** | 零代码 |
| 无批量 API | ✅ **正确** | 无 `batch` 端点 |
| `aero-cli` 只有 migrate | ✅ **基本正确**（有 health/ai-test/streams/ws-ping 但无 import/export） | 检查源码确认 |
| 无标准化导出格式 | ✅ **正确** | `me_export.rs` 有功能性导出但无标准 Schema |
| 跨工作区迁移 | ✅ **正确** | 无实现 |

### 方向四：生产级 Web SPA ⭐⭐⭐⭐⭐

| 声明 | 验证 | 证据 |
|------|------|------|
| debug client 标志 | ✅ **准确** | `index.html` 第 19 行：`<p class="muted">debug client · 联调专用</p>` |
| ~5.9K 行 | ✅ **准确** | `wc -l` = 5939 |
| 搜索 UI 缺失 | ❌ **错误** | `web/search.js`（144 行）实现基础搜索（`mode: 'auto'`）+ AI 问答 |
| 无保存搜索/高级操作符 | ✅ **正确** | `search.js` 无 `saved_searches`、无 `from:`/`in:`/`before:` |
| 仅 2 处 aria | ⚠️ **估算偏低** | 实际 4 处：`aria-live`(1) + `role="tablist"`(1) + `role="tab"`(2) |

**额外发现**：
- 文档覆盖表的"已覆盖/未覆盖"划分大体准确。但搜索 UI 应移至"已覆盖"（基础功能）
- `web/manifest.json` 存在——PWA 基础设施有最小启动配置（文档说零）
- SPA 代码比文档描述的结构更清晰：模块化 ES2020 imports（`api.js`/`context.js`/`render.js` 等），不是纯单体

### 方向五：秘密管理 ⭐⭐⭐⭐

| 声明 | 验证 | 证据 |
|------|------|------|
| 无秘密源抽象 | ✅ **正确** | 无 `SecretStore` trait，无 `aero-secrets` crate |
| 无密钥轮换 | ✅ **正确** | `config.toml` 包含静态 RSA 密钥，无轮换逻辑 |
| 无秘密审计 | ✅ **正确** | 无秘密访问日志 |
| TOTP 明文 | ✅ **正确** | `text NOT NULL` |
| `config.toml` 明文 PEM | ✅ **更严重** | 包含真实 RSA 私钥 |

**纠正**：文档说 `jwt_secret = "change-me"` 在 `config.example.toml` 第 1-10 行——**不准确**。实际配置使用 RS256 非对称密钥对，`config.example.toml` 使用 `REPLACE_WITH_...` 占位符。但 **`config.toml`（非 `.example`）包含真实 RSA 私钥**，这个发现比文档声称的更严重。

---

## 核心结论

| 维度 | 评估 |
|------|------|
| **5 个方向的战略价值** | ✅ **全部获得确认**——每个方向在代码库中零系统性分析 |
| **代码证据准确性** | ⚠️ 方向一有**文件路径错误**（3 处路径不对），方向四有一处覆盖分类错误（搜索 UI 实际存在），方向五的配置描述不精确 |
| **事实发现质量** | ⭐⭐⭐⭐——分析框架扎实，但低估了一处（方向一实际的 BWE 基础设施比描述完善）并高估了一处（方向四搜索 UI 实际存在） |
| **建议可行性** | ⭐⭐⭐⭐⭐——Phase 划分务实，ROI 评估合理 |
| **最大低估** | 方向一实际有 `BandwidthEstimator` + `PublisherRembAggregator` + `ThroughputEwma`——缺失的是**计费级**核算而非带宽估计本身 |
| **最大高估** | 方向四的搜索 UI 被列为"未覆盖"——实际基础搜索可用 |
| **最严重遗漏** | 文档未提及 `config.toml`（非 `config.example.toml`）中包含真实 RSA 私钥——这是即时的安全事件 |
