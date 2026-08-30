# Aero Platform 本地部署

该编排把 Snaplink、Snaplink Console、Audit Governance、Aero ID、Aero
Vault、Aero IM 和 Iris UI 平台控制台放进同一个本地 Docker 平台。人类登录统一走
Snaplink OAuth 2.0/OIDC；服务间调用统一用 Snaplink `client_credentials`；文件走
Aero Vault；聚合账户走 Aero ID；Aero ID 与 Aero Vault 审计写入 Audit Governance。
Aero IM 通知以无消息正文的有界投影进入 Aero ID，Iris UI 只读展示并把处理动作留在
Aero IM，避免 Aero ID 成为第二个通知权威。
Aero Vault 的租户、桶名称和配额用量也通过 Aero ID 的 allow-list 投影进入 Iris UI；文件内容、
下载凭据和所有存储写操作仍由 Aero Vault 权威处理，浏览器不会把 Aero ID token 转发给 Vault。
Snaplink 的身份、安全状态、租户和授权信息同样通过 Aero ID 的显式 allow-list 只读展示；密码、
MFA、会话与角色变更仍进入 Snaplink 管理端，控制台不展开来源返回的未知字段。

## 启动

从 `aero-im` 仓库根目录执行：

```bash
cp deploy/platform/.env.example deploy/platform/.env
# 修改 deploy/platform/.env 中的全部 change/replace 值
docker compose --env-file deploy/platform/.env \
  -f deploy/platform/compose.yaml up -d --build
docker compose -f deploy/platform/compose.yaml ps
```

首次构建会下载 Rust、Go、Node、Postgres、MinIO、NATS 和 Redis 镜像，耗时取决于
Docker Hub 与语言包代理网络。编排只默认绑定 `127.0.0.1`：

| 入口 | 地址 |
|---|---|
| Iris UI 平台控制台 | <http://localhost:28010> |
| Snaplink 管理端/登录页 | <http://localhost:28000> |
| Snaplink issuer | <http://localhost:28080> |
| Aero Vault API | <http://localhost:28081> |
| Aero IM | <http://localhost:28082> |
| Aero ID API | <http://localhost:28083> |
| Audit Governance API | <http://localhost:28089> |
| MinIO Console | <http://localhost:29001> |

本地初始 Snaplink 用户为 `admin`，密码取 `.env` 的
`SNAPLINK_ADMIN_PASSWORD`。Iris UI 使用 Authorization Code + PKCE 客户端
`aero-account-console` 登录，不在浏览器里保存任何机器客户端密钥。
`aero-platform-console` 的 `/healthz` 只检查 Web 容器本身；可用
`docker compose -f deploy/platform/compose.yaml ps` 查看其 `healthy` 状态。

Snaplink 的 `security.cors.allowed_origins` 必须同时包含 Iris UI 控制台 origin 和
Hosted Login origin：控制台负责发起授权与交换 code，Hosted Login 则从自己的 origin
向 Snaplink 提交凭据。漏掉后者会使登录页停留原地，并由 Snaplink 以 403 拒绝请求。
Snaplink Console 构建显式从镜像内的 `/canvaskit` 加载 Flutter renderer，Hosted Login
不会因外部 renderer CDN 不可达而白屏。

当前六项目范围不包含 Billing 与 Stripe Adapter。Snaplink Console 对这两类可选路由
使用 `optional-api-disabled` 明确返回 HTTP 501，避免误转发到 Snaplink 核心服务；接入
真实商业服务后应把两个 `SNAPLINK_*_UPSTREAM` 改为其独立 origin。

## 验证

```bash
python3 -m unittest discover -s deploy/platform/tests -v
docker compose -f deploy/platform/compose.yaml config --quiet
curl -fsS http://localhost:28080/readyz
curl -fsS http://localhost:28089/readyz
curl -fsS http://localhost:28083/readyz
curl -fsS http://localhost:28083/health/sources
```

`health/sources` 应同时返回 Snaplink、Aero IM、Aero Vault 为 `ok`。Aero ID 的通用
connector 固定探测 `/healthz`，平台 loopback edge 分别把它映射到 Snaplink `/readyz`
和 Aero IM `/health/ready`；Aero IM 的结果包含 PostgreSQL、Redis、NATS 与 Vault blob
依赖，不能用始终 200 的静态响应代替。

本地夹具的 Aero IM account-summary OAuth resource 已对齐为 `aero-im`；通用集成
受众仍是独立的 `aero-im-integration`。本地未配置专用 target-assertion issuer、
audience、subject 或 JWKS，因此 `/internal/account-summary` 有意保持 fail-closed
（502）。启用必须由外部 HTTPS 专用 JWKS 与协调后的 Secret/config rollout 完成。

`audit-bootstrap` 是 create-only 的一次性控制器：它注册 `platform-local`
tenant、Aero ID/IM/Vault source 和对应 schema；重复启动时 HTTP 409 被当作幂等成功。
Vault 与 Aero ID 审计 relay 默认启用并采用 fail-closed 绑定。

`snaplink-config-sync` 通过 Snaplink 管理 API 幂等同步已有 SQLite 数据卷中的
`aero-account-console` scope。这样后续新增 scope（当前包括 `audit:read`）无需删除
身份、会话或 OAuth 数据卷；Snaplink 配置同时把本地 `admin` 映射到
`sso-admin-console` 的管理角色和 `aero-id` 的平台管理员角色，Aero ID 的
`Authorizer.Check` 仍以 Snaplink 为唯一授权判定源。

## 当前审计边界

Aero ID 与 Aero Vault 已直接把审计事实发送到 Audit Governance。Aero IM 的
Audit Governance relay 受其商业 workspace 绑定（Q0）保护；不能只靠环境变量绕过，
需在创建真实 workspace 并完成商业绑定后按 Aero IM 的审计 provision 流程启用。
Snaplink 自身仍以启用 hash-chain 的 SQLite audit ledger 作为本地耐久事实源；当前
stock `sso-server` 未把全部原生 audit record 接到 Audit Governance relay，因此不能把
“所有 Snaplink 原生审计已集中化”作为生产验收结论。

## 生产化要求

该文件是单机开发/验收拓扑，不是生产安全配置。生产至少要替换全部静态密钥、启用
HTTPS 和可信域名、使用外部 Secret Manager、限制 MinIO/数据库网络、配置对象锁或
WORM 审计归档，并完成公网 TURN、跨节点 RTP、OIDC/SMTP/推送等 staging 联调。
