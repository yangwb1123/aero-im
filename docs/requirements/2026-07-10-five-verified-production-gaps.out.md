您的分析非常扎实。我已经对代码库进行了交叉验证，以下是逐项确认结果和补充发现：

---

## ✅ 验证结果

### 方向一（SSRF）— **基本成立，但有重要更新**

**unfurl_bot 的 SSRF 缺口仍然存在** — 您在 `ReqwestUnfurler::fetch()` (`crates/aero-storage/src/unfurl.rs:396-413`) 发现的裸 `client.get(url).send().await` 无任何私有IP校验、无重定向链限制。唯一防线是 `Content-Type: text/html` 检查——可以被恶意服务器轻松绕过。

**但 webhook SSRF 已在 #c9d5141 修复**（比本文件早 20 天提交）：`webhooks.rs:206-275` 新增了 `webhook_ip_is_blocked()` + `assert_webhook_url_safe()`，在创建 `outgoing_webhook` 时拦截：

```rust
// webhooks.rs:206
fn webhook_ip_is_blocked(ip: std::net::IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local()
                || v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => { /* IPv4-mapped, loopback, ULA, link-local checks */ }
    }
}
```

**仍然存在的缺口**：
- ✅ `ReqwestUnfurler` 完全无防护（您已指出）——这是 unfurl_bot 的 SSRF
- ⚠️ `ReqwestSender::deliver()` 投递时不做重新验证——只能在创建时阻，如果 DNS 记录在 webhook 存活期间变更（DNS rebinding）则无法防御（commit message 已标记为 known follow-up）
- ❌ 两个出站 HTTP 路径都无重定向链限制（`redirect::Policy::limited(5)`）

### 方向二（负载测试）— **完全成立**

```bash
$ grep -c "criterion\|k6\|locust\|wrk\|gatling" Cargo.toml crates/*/Cargo.toml
# → 全部 0
```

`criterion` 在 16 个 crate 的 `Cargo.toml` 中零出现。无 `tests/load/`、无 `tests/chaos/` 目录。唯一接近"benchmark"的是 `aero-live-srt/src/pump.rs` 中 `Sink` trait 的文档注释——仅注释，无实际 benchmark。

### 方向三（部署成熟度）— **完全成立**

```bash
$ ls -la Dockerfile* k8s/ 2>/dev/null; echo $?
# → No such file or directory (exit 2)
```

零 Dockerfile、零 k8s manifest、零 helm chart。唯一的部署载体是 `docker-compose.yml`（5 个依赖服务，不含 server）。

### 方向四（事务性邮件）— **完全成立**

```bash
$ wc -l crates/aero-server/src/mailer.rs
# → 125
```

文件内方法：`build_mailer()` + `send_password_reset()` + `send_invitation()` = 精确 3 个函数。同步 SMTP、纯文本、无队列、无模板、无退信处理。代码注释的第 8 行自己承认了所有局限。

### 方向五（API 版本化）— **完全成立**

```bash
$ grep -c "/api/v1/" crates/aero-server/src/routes/routes.rs
# → 0
```

2854 行的 `routes.rs` 所有路由以 `/api/...` 起始，无版本前缀。OpenAPI 文档虽然有 `/api/openapi.json` 端点，但仍是静态手写（您已指出）。

---

## 🔍 额外发现（文档未覆盖）

1. **跨方向关联**：方向一（SSRF）的 A 期方案提到的 `SafeHttpClient` wrapper——commit `c9d5141` 实际上已经部分实现了这个模式（`webhook_ip_is_blocked` + `assert_webhook_url_safe` 可以用作基类）。约 100 行现有代码可复用，迁移成本更低。

2. **邮件方向的额外证据**：`send_password_reset()` 的错误处理值得注意——
   ```rust
   // mailer.rs:59
   pub async fn send_password_reset(&self, to_addr: &str, token: &str) {
       // → 返回 ()
   ```
   返回 `void`，即使 SMTP 中断调用者也得不到任何信号。这与 AGENTS.md §4.3 的「at-least-once 状态机」原则相悖——密码重置是不可重试的单向依赖。

3. **邮件队列的可测试性**：`webhook/delivery.rs` 的 `FakeSender` seam（`delivery.rs:185`）是可以直接复用的模式——邮件方向的 `FakeMailer` 同理。

4. **负载测试的第一可行锚点**：`aero-bus` 的 events 编码/解码是最适合 criterion bench 的热路径——无外部依赖、纯 CPU 计算、调用频次高（每条消息至少一次编码+一次解码）。

---

## 📋 优先级调整建议

| 方向 | 您给的优先级 | 建议调整 | 理由 |
|------|-----------|---------|------|
| SSRF（unfurl_bot 剩余部分） | P0 | **P0** → **紧急（本周）** | webhook 已修，但 unfurl_bot 的攻击面更大（用户消息 URL 触发 vs 管理员配置）。且 100 行 webhook SSRF 代码可以直接复用 |
| 负载测试 | P0 | **P0** | 不变。815 个单元测试后无任何容量数据的观点非常有力 |
| 部署成熟度 | P1 | **P0**（前移） | 无 Dockerfile = 无法容器化部署 = 企业 PoC 的阻塞项——比邮件优先级更高 |
| 事务性邮件 | P1 | **P1** | 不变。PMF 前的日活杠杆，但不是阻塞性 |
| API 版本化 | P2 | **P2** | 不变。在 bot/SDK 生态启动前是增值而非必要条件 |

**总的建议**：方向一（unfurl_bot SSRF）复用已有 `webhook_ip_is_blocked` 代码 ≈ 2 小时工程。方向三（Dockerfile）≈ 1 天。这两条做完后，方向一的 P0 可移到 A 期完成，方向三的 P0 可开始。方向二的 k6/criterion 可以和方向四的邮件队列并行推进。

需要我展开某个方向的实施方案吗？
