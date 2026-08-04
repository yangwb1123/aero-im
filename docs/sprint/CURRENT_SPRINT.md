# CURRENT_SPRINT.md — 当前实现收口看板

> 本文件只记录当前源码可证明的状态，不保存会漂移的迁移、测试或文件行数。
> 开始任务前先检查工作树、`README.md`、`docs/SESSION_HANDOFF.md` 和相关源码；
> 历史阶段名不能替代现场验证。

## 当前目标

- 状态：功能实现收口与验证。
- 优先级：先修复源码与文档不一致，再处理真实缺陷，最后补充明确授权的新能力。
- 完成标准：实现、鉴权、事务/幂等、实时或后台接线、Web 消费面和相应测试一起闭环。

## 已落地的当前能力

| 能力域 | 当前源码事实 | 主要复核锚点 |
|---|---|---|
| 消息可靠性 | 创建、编辑、删除以消息聚合版本写入事务 outbox；通知和 AI 后置工作与业务状态一起提交；REST/WS 发送共享校验与幂等语义；外部副作用 consumer 使用带租约和 fencing 的 durable receipt | `ImService`、`event_outbox`、`message_side_effect`、`ConsumerEventReceiptRepo` |
| 事务与资源围栏 | 定时/周期消息、MLS 不透明中继与关键词提醒、预测、置顶、直播治理、直播目标和预约直播均在数据库提交点复检 actor、租户/房间/stream 归属、生命周期与配额；活跃工作区在 participant、TOTP、成员角色或强制 2FA 跨行变更提交后仍须保有有效 Owner；应用层按规范化顺序取锁，raw SQL 不能绕过关键边界 | `scheduled`、`recurring`、`mls`、`keyword_alert`、`predictions`、`pin`、`stream_mod`、`raids`、`goals`、`scheduled_stream`、`aero_workspace_has_effective_owner` |
| AI 行动项持久化 | `POST /api/rooms/:id/action-items?persist=true` 要求 `Idempotency-Key`；同一 actor/room/idempotency-key digest 原子创建批次与任务，重放返回原任务 ID，空结果也有 durable receipt，并发重试不会拼接不同批次；账户擦除保留共享任务，仅解绑 batch key/index 并删除私有 receipt/digest | `action_items.rs`、`task/action_item_batch.rs`、`ParticipantRepo::delete_participant` |
| 搜索反馈 | 高级搜索由服务端保存短期 impression、完整规范化请求与有序结果快照；点击只提交 `impression_id` 与 `result_id`，仓储推导 query/rank、复检当前访问权并限制每个 impression 一次确定性点击 | `search_advanced.rs`、`search_feedback.rs` |
| 保存搜索监控 | Owner-scoped 保存搜索支持启停监控；首次启用以当前时间为基线，不回灌历史；后台 worker 用 `(created_at,message_id)` 复合游标、有界 keyset 分页和稳定通知 ID，在并发实例及崩溃重试下收敛 | `saved_searches.rs`、`saved_search_monitor.rs`、`saved_search/` |
| 直播治理与申诉 | moderator/ban/raid 等写路径在 stream 围栏下复检当前权限；ban revision 标识具体封禁代际。被封禁用户可 `POST /api/streams/:id/appeals`，主播/当前 moderator 可读取队列并通过 `POST /api/appeals/:id/review` 审核；旧申诉不能解除后续重新建立的封禁 | `stream_mod.rs`、`stream_moderators.rs`、`raids.rs`、`ban_appeals.rs` |
| Agent/Bot 租户安装 | `/api/agents` 必须显式给出 `room_id`，仅当前 room manager 可创建；participant、workspace membership 与 room membership 原子提交后发送 `MemberAdded`，direct/标记 group DM 零写拒绝。`/api/bots` 提供 `workspace_id` 时把 participant、普通 workspace membership、Bot registry 与 token hash 原子提交，省略时为个人/system Bot；workspace Bot membership backfill 修复存量 live scoped Bot 的成员边，同时保留已有更高角色 | `create_service_identity_authorized`、`create_authorized_with_token`、`migrations/*_bot_workspace_membership_backfill.sql` |
| 企业 Web 前台 | Web SPA 已提供治理、企业安全、企业合规三个管理面：审计与 Bot、会话/2FA/存储区域/IP allowlist/SCIM/AutoMod/IdP、留存/导出/法务保全/信息隔离/成员生命周期/邀请/Webhook。前端只做可用性门控，后端仍按当前 Owner/Admin 和资源归属复检 | `web/governance.js`、`web/security_admin.js`、`web/compliance_admin.js` |
| SFU 与跨节点媒体 | 浏览器 `call_sfu_v2` offer/ICE/subscription 已进入生产 WS lifecycle，并驱动 `SfuMediaRegistry` bind/run；持久 call-leg generation 经 PG/Redis CAS、WS/SFU 事件和精确清理围栏旧连接，legacy caller reconnect 兼容迁移仍对伪造 caller fail-closed。本地 RTP 进入 `SfuForwarder` 和 `CallEgress`；supervisor 已接 `ensure_bridges`/`ensure_egress`，内部 subscribe/feedback 端点以共享 secret 保护；真实旧 v3 / 当前 v4 二进制已通过双向媒体与协议降级验收 | `ws/ws_impl/sfu.rs`、`sfu_media.rs`、`call_bridge_supervisor.rs`、`call.rs`、`call_route.rs`、`live_presence.rs`、`web/sfu_calls.js` |

通知聚合、部分索引、富文本与搜索高亮、多级缓存、SMTP、安全响应头和
Bot 开放平台也已落地；需要细节时以对应模块和 `README.md` 功能矩阵为准。

## 明确尚未完成的环境验收

以下项目不能因结构测试、协议测试或 localhost 测试通过而标记为生产完成：

| 项目 | 仍需完成 |
|---|---|
| 浏览器 WebRTC | 首 offer 预留 7 对 recvonly 槽覆盖默认 8 人群规模；当前 generation-fenced 构建已通过本机真实 Chrome 双客户端、late-subscriber、跨 gateway 重连，真实 Firefox 双向音视频和本机 coturn 强制 relay-only；物理设备、Safari、跨主机/公网 NAT/公网 TURN、长时弱网与超出默认规模的 Firefox 实机扩容仍待 |
| 跨主机 call-bridge | 本机两个独立 gateway 的 generation-bound 双向 RTP 与旧连接清理围栏已通过；可路由 advertise 地址、UDP 可达性、NAT/防火墙、sticky route 与跨主机双向 RTP/RTCP 仍待 |
| 直播摄入/播放 | 真实 ffmpeg RTMP/WHIP/加密 SRT（含 post-handshake SEK 轮换）、真实 OBS Studio RTMP 与 Chrome WHEP 播放已通过；Safari 播放、真实推流设备与跨网部署仍待 |
| 外部服务 | 本机 MinIO/Mailpit/mock OIDC/Jaeger/ClamAV/OTel 已通过；真实外部 S3/KMS、FCM/APNs、SMTP/OIDC/OTLP 等供应商凭据网络往返仍待 |
| SAML | 默认 ACS 继续 fail-closed；生产启用前须接入并安全评审经审计的 XML-DSig verifier |
| 消息分区切换 | shadow、回填与 runbook 已准备；生产表交换仍需获批维护窗、写入闸门、备份/恢复演练、DBA 执行与回滚方案 |

MLS 客户端密码学、联邦和原生移动 SDK 是既定非目标，不应作为当前 Sprint
的待实现项。

## 验证纪律

迁移会编译进 `aero-cli`；任何迁移变化都必须先 build，再在全新一次性数据库
执行 migrate。不要修改共享开发库或手工改 `_sqlx_migrations`。

```bash
cargo build --workspace
cargo check --workspace
cargo test --workspace --lib
cargo clippy --workspace --all-targets
scripts/truth-check.sh
scripts/file-size-check.sh
scripts/web-check.sh
```

PG 门控测试使用指向已完整迁移的一次性数据库的 `DATABASE_URL`。运行时 REST/WS
smoke 也应使用同一隔离库，服务停止后再删除。

## Agent 决策流程

1. 读取本文件、`README.md`、`docs/SESSION_HANDOFF.md` 和当前 `git status`。
2. 用 `rg` 找到路由、仓储、迁移、boot/worker、WS/Web 消费面的真实调用链。
3. 若文档与源码不一致，先按源码修正文档；若源码有缺口，再实现最小完整闭环。
4. 执行与风险相称的目标测试和上述静态门禁。
5. 只有真实环境完成验收后，才能关闭对应 staging 项。
