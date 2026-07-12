已保存。文件 `docs/requirements/2026-07-11-round-30-global-scan-five-uncovered-strategic-expansion-directions.md` (15 KB)。

总结这 5 个方向及其与 49+ 既有分析的关键区分：

| # | 方向 | P | 核心主张 |
|---|------|---|---------|
| 1 | **备份与灾难恢复** | **P0** | Postgres/Redis/NATS/JWT 密钥全无备份 — `docker-compose down -v` 即全量丢失 |
| 2 | **媒体资产管线** | P1 | 图像零处理（无 `image-rs`）、零 CDN、EXIF 裸奔、HLS 直出 — 加载慢 + 隐私泄露 |
| 3 | **支付处理管线** | P1 | `subscription_tiers.price_cents` 已定义但从未扣款；虚拟经济无真实货币锚定 |
| 4 | **移动端战略** | P1 | `aero-push` 推送管线就绪但无客户端注册 token；Web 无 viewport/responsive/PWA |
| 5 | **负载测试框架** | P1 | 841 个 UT 全绿但零负载测试 — 无 k8s/o11y 但有 k6/oha 可选 |

编号 `round-30` 接续既有序列（目录中已有 round-8/15/16/18/32 等），与既有文档不重叠——已逐方向交叉验证。
