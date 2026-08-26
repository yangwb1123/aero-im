# PostgreSQL 备份与恢复演练

本 runbook 覆盖 Compose 部署的 PostgreSQL 逻辑备份、一次性恢复校验和灾备切换边界。
仓库脚本默认只执行安全的“备份”和“恢复到新库”流程，不会原地覆盖业务库。

## 数据与安全边界

- 归档包含消息、身份、审计及其他个人数据，应按生产机密处理。脚本以 `0600` 创建文件和
  SHA-256 sidecar，但校验和不是加密；生产环境必须在离机前使用部署方 KMS/密钥加密。
- 默认目录 `data/backups/postgres/` 只是本地落点，不是灾备存储。部署方必须配置异地、
  跨故障域的对象存储、版本保留和不可变/防删除策略，并定期测试解密权限。
- `pg_dump --serializable-deferrable` 提供 PostgreSQL 内部一致快照，但不会同时冻结 S3/MinIO
  blob、NATS JetStream 或 Redis。附件恢复需要同一恢复点附近的对象存储版本；Redis 中的
  presence/roster 可由心跳重建，NATS 的 stream/consumer 状态须按部署平台另行备份。
- 逻辑备份不能替代 PostgreSQL WAL 归档/PITR。若生产 RPO 小于备份周期，必须启用并监控
  base backup + WAL archive，且以同样方式做恢复演练。

## 创建归档

Compose 默认容器名、数据库和用户分别为 `aero-postgres`、`aero`、`aero`：

```bash
make postgres-backup
```

成功后会输出 `.dump` 与 `.dump.sha256` 的绝对路径。写入先落到同目录临时文件，归档可由
容器内匹配版本的 `pg_restore --list` 解析且校验和生成后才原子发布；已有目标文件会被拒绝。

定制目标或连接参数：

```bash
AERO_BACKUP_POSTGRES_CONTAINER=aero-postgres \
AERO_BACKUP_DATABASE=aero \
AERO_BACKUP_USER=aero \
make postgres-backup BACKUP=/secure/backups/aero-$(date -u +%Y%m%dT%H%M%SZ).dump
```

计划任务应记录脚本退出码、归档大小和校验和，并对“未生成新备份”“大小异常”“离机复制失败”
告警。删除旧归档前先确认保留策略和法务保全要求，不能只按本机磁盘空间滚动删除。

## 一次性恢复校验

```bash
make postgres-restore-verify DUMP=/secure/backups/aero-20260822T120000Z.dump
```

校验按以下顺序执行：

1. 若 sidecar 存在，先校验 SHA-256，并解析 custom-format 目录。
2. 新建随机的 `aero_restore_verify_*` 数据库；目标已存在即失败，绝不复用。
3. 在单事务中恢复，检查核心表、成功迁移账本非空且不存在失败迁移。
4. 成功或失败后删除本次新建的校验库。

需要人工取样或运行额外查询时，可保留恢复库：

```bash
AERO_RESTORE_VERIFY_DATABASE=aero_restore_verify_ticket_1234 \
AERO_RESTORE_KEEP_DATABASE=1 \
make postgres-restore-verify DUMP=/secure/backups/aero-20260822T120000Z.dump
```

检查完成后由操作者显式删除：

```bash
docker exec aero-postgres dropdb -U aero --force aero_restore_verify_ticket_1234
```

## 灾备恢复与切换

生产事故中仍应恢复到一个**新数据库**，不要对受损或仍服务流量的源库执行原地 restore：

1. 摘流并记录事故时间、目标 RPO/RTO、选定归档/WAL 恢复点及所有校验和。
2. 在隔离实例创建新库，使用与备份兼容的 PostgreSQL/扩展版本恢复；不要先运行新迁移。
3. 核验 `_sqlx_migrations`、核心行数、审计/outbox backlog、附件引用及抽样业务数据。若使用
   PITR，再核验恢复时间线和 replay LSN。
4. 对恢复库构建并运行匹配该迁移账本的应用版本，执行 `/health/ready` 与关键读写 smoke。
5. 再切换 `AERO__DATABASE__URL`，逐步恢复 worker/入口流量，观察错误率、outbox、NATS backlog
   和重复外发。外部副作用依赖持久幂等 receipt，但切换后仍须重点观察。
6. 保留旧库只读证据和完整操作日志；稳定后按批准的保留/销毁流程处理，禁止临场直接删除。

如果灾备同时恢复 Redis，须确保 `bus/seq.rs` 使用的 per-subject 序列不会回退；不确定时先保持
入口摘流并由工程负责人制定序列修复方案。对象存储恢复也必须先于开放附件写入完成引用核对。

## 验收与证据

`make docker-smoke` 会在独立容器中迁移全新数据库，写入三行标记，创建归档，恢复到新库，
并比较标记数据与 `_sqlx_migrations` 校验和指纹。它是可重复的仓库级回归，不代表生产数据量、
PITR、KMS、异地存储或真实故障切换已经验收。

每次 staging/生产演练至少保存：归档/恢复点标识、校验和、开始与结束时间、恢复数据量、
实测 RPO/RTO、核验查询结果、应用 smoke 结果、异常与责任人。季度演练及告警阈值由部署方 SLO
决定；未取得这些环境证据时，交付清单中的 staging 备份恢复项保持未完成。
