# Aero IM

AI-Native 即时通讯 + 直播平台,Rust 实现。

- 设计:[`docs/specs/2026-05-22-aero-im-design.md`](docs/specs/2026-05-22-aero-im-design.md)
- 状态:P0 进行中

## 快速开始

```bash
# 1. 起开发环境(Postgres/Redis/NATS/Jaeger/MinIO)
docker compose up -d

# 2. 跑迁移
cargo run --bin aero-cli -- migrate

# 3. 启动服务
cargo run --bin aero-server

# 4. 打开浏览器
# http://localhost:3000
```
