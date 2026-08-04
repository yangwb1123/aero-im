# Aero IM — 当前交付清单

> 这里仅记录尚需外部环境或运维授权的验收，不保存会漂移的迁移序号、
> 功能数量、测试数量或源码行号。产品能力以
> [`README.md`](../../README.md) 的功能矩阵为准，架构与工程约束以
> [`AGENTS.md`](../../AGENTS.md) 为准，实施状态以
> [`ROADMAP.md`](../ROADMAP.md) 和当前源码为准。

## 当前源码状态

安全、核心用户旅程、持久投递、企业治理前台、媒体生产生命周期、资源约束和
可靠性链路均已进入当前工作树及自动门禁。以下曾经的 backlog 已不再成立：

- fresh-deploy 迁移链、ignored PostgreSQL 集成测试和 CI 服务依赖已有自动门；
- `NotifyBatch` 使用确定性 delivery id，外发消费者另有持久 receipt/outbox；
- 跨房 AI 画像已有 workspace-scoped、默认关闭的 opt-in 实现；
- `messages_partitioned` shadow、增量 backfill、cutover 与 rollback runbook 已就绪；
- HTTP/WS 租户指标通过 handler 写入的有界 workspace marker 按需启用；
- room-scoped Agent/Bot 由 `create_service_identity_authorized` 在 manager 复检下原子
  创建 participant 与 workspace/room 成员边并发送 `MemberAdded`；workspace-scoped
  Bot 由 `create_authorized_with_token` 原子创建 participant、普通 workspace
  membership、registry 与 token；
  `migrations/*_bot_workspace_membership_backfill.sql` 已修复存量 live scoped
  Bot 且不降级已有更高角色；
- Webhook、Bot、推送和媒体会话均有容量边界、代际围栏与 shutdown 收口。
- 当前 generation-fenced 二进制已在一次性库通过真实 Chrome 本机双 gateway
  late-subscriber 双向 RTP，以及同参与者跨 gateway 重连、旧连接延迟清理和
  替代媒体持续增长验收；真实旧 v3 / 当前 v4 二进制也已通过双向音视频和
  wire-version 降级验收。
- Chrome WHEP、Firefox SFU、本机 coturn 强制 relay-only、真实 OBS Studio
  RTMP，以及 ffmpeg RTMP/WHIP/加密 SRT（含服务端 SEK 周期轮换）均已通过。

若这些事实发生变化，应修改对应源码、测试和权威文档，不要在这里复制计数。

## 仍需真实环境验收

- [ ] 在物理设备、Safari、跨主机/公网 NAT/公网 TURN、长时弱网下完成
  摄像头/麦克风权限、渲染、重连及更广多人通话验收；本机 LAN coturn
  强制 relay-only 和虚拟媒体通过记录不能替代这些项目。
- [ ] 两个跨主机真实节点完成 advertised address、Redis route、UDP/NAT/防火墙
  以及双向 RTP/RTCP 验收；本机两个独立 gateway 的通过记录不能替代此项。
- [ ] 使用部署方真实供应商凭据完成外部 S3/KMS、FCM/APNs、SMTP、OIDC、
  OTLP、AI provider 与 GIPHY 网络往返；校准账单/质量，并人工确认 GIPHY
  attribution 在目标浏览器和 CSP 下可见。本机 MinIO/Mailpit/mock
  OIDC/Jaeger/ClamAV/OTel 的通过记录不能替代供应商验收。
- [ ] 在 staging 验证 Prometheus 告警、仪表盘、备份恢复和容量/SLO。

这些项目需要真实对端、网络、凭据或人工观察；hermetic CI 不能替代，也不能在
未执行时写成“已完成”。

## 仍需运维授权

- [ ] 生产 `messages` 分区切换：维护窗、写入门、备份恢复演练、全量 parity、
  DBA 执行 checked-in cutover/rollback 脚本及 soak 后清理。
- [ ] SAML 生产启用：当前实验路径已有严格结构/算法预检、条件校验和一次性请求
  消费，但仍须接入经过审计的 XML-DSig verifier 并完成安全评审。默认 ACS
  继续 fail-closed；实验 verifier 不构成生产验收。
- [ ] 活跃跨节点通话的生产混合版本发布：本机真实 v3↔v4 双向媒体兼容已经
  验收；生产 rollout 仍需按变更窗观测。发布链若仍含无绑定 v2，按 runbook
  drain/reconnect，不能把单向 v2 兼容宣称为无损滚动升级。

## 已确定的产品边界

- 用户 block 的既定语义是阻止新 DM、1:1 通话和通知；共享房间内容不做全局
  隐身/删除。若产品要改为 full cloak，必须单独定义兼容与审核策略。
- SCIM 仅入站供给；VOD 复用现有 HLS；OpenAPI 是示意性文档。
- MLS 客户端密码学、联邦和原生移动 SDK 是明确非目标。

## 每次交付的证据门

迁移变更必须先重新构建，再在全新一次性 PostgreSQL 数据库迁移。随后运行：

```bash
cargo check --workspace --all-targets --locked
cargo test --workspace --lib --locked
cargo clippy --workspace --all-targets --locked
cargo test -p aero-server --test authz_lint --locked
scripts/truth-check.sh
scripts/file-size-check.sh
scripts/web-check.sh
```

ignored DB/Redis/NATS 测试与 REST/WS smoke 必须使用隔离数据库；服务停止后删除
该数据库。CI 配置位于 `.github/workflows/ci.yml`，其动态比较迁移源文件与
ledger，不在文档中硬编码数量。
