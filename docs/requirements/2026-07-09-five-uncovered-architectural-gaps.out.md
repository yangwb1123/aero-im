现在我已经有了完整的代码验证。让我给你一个详细的评估反馈。

---

## 代码验证评估反馈

我对照了 16 个 crate 的实际源码，验证了你这份分析文档的核心主张。结论是：**5 个方向的选择方向正确，但文档中多个关键代码主张与事实不符。** 以下逐方向拆解。

---

### 方向一（搜索体验面）——⚠️ 多处严重代码误判

**「结果高亮摘要：需要 `ts_headline()` 或后端计算 snippet」——❌ 错误**

`crates/aero-storage/src/search_query.rs` **已经在使用 `ts_headline()`**（第 235 行）：

```sql
ts_headline('english', m.searchable_text, websearch_to_tsquery('english', f_unaccent($2)),
  'StartSel=<b>, StopSel=</b>, MaxWords=50, MinWords=15, ShortWord=3') AS headline
```

后端返回 `SearchHit { headline: Option<String>, ... }`（第 55/152 行）。响应中已有 `"headline": ...` 字段（`search_advanced.rs` 第 148 行）。**真正的缺口在前端**——`web/search.js` 渲染搜索列表时完全忽略了 `h.headline`（第 38 行直接用 `blocks` 全文渲染）：

```js
// search.js:38 — 不渲染 headline！
text.textContent = (m.blocks || []).map((b) => b.content || '').join(' ').slice(0, 240);
```

**「拼写纠错/「你是不是要找...」——完全缺失」——❌ 错误**

`crates/aero-server/src/search_advanced.rs` 第 100-112 行**已经实现**：

```rust
let suggestions = if after.is_none() && total < 3 && !term.is_empty() && !term.contains(' ') {
    repo.suggest_terms(auth.participant_id, workspace, term, 5)
        .await.unwrap_or_default()
} else { Vec::new() };
```

响应中已包含 `"suggestions": suggestions`。**前端不渲染它。**

**「分面过滤——完全缺失」——❌ 错误**

同一文件第 114-122 行**已经实现**：

```rust
let facets = if req.facets && after.is_none() {
    Some(repo.facets(auth.participant_id, workspace, &parsed, FACET_TOP)
        .await.map_err(AeroError::from)?)
} else { None };
```

响应中已包含 `"facets": facets`。`req.facets` 是一个可选的 bool 参数。**前端不渲染它。**

**「搜索即搜即现/自动补全」——✅ 准确**

确实不存在 `GET /api/search/suggest` 端点。`suggest_terms` 只在完整搜索路径的低结果场景中被调用，不提供实时的 typeahead API。

> **修正建议**：方向一改题为「搜索前端 UI 管线——后端能力已就位（headline/suggest/facets），前端未渲染使用」，预估工作量从 ~11 天减为 ~5 天。

---

### 方向二（持久层阻尼）——✅ 大致正确，但有遗漏

**「DB 断路器」——领域正确**。Webhook 已有完整断路器（`breaker.rs`，`BREAKER_FAILURE_THRESHOLD=5`），但 DB/PG 池确实没有。你建议的 `CircuitBreaker<PgPool>` 包装是新内容。

**「S3 重试」——⚠️ 已存在但无 fallback**。`s3_blob_store.rs` 第 175-223 行已有指数退避重试：

```rust
let mut backoff = std::time::Duration::from_millis(100);
// ... retry loop with backoff ...
tokio::time::sleep(backoff).await;
```

但确实没有 fallback 到 LocalFs。

**「NATS publish 背压」——✅ 准确**。不存在。

> **修正建议**：更新 S3 重试部分的描述，去掉「S3 无重试」的说法。

---

### 方向三（线程化）——⚠️ HTML 结构描述有误

**「index.html 第 260 行 = `<aside id="thread-panel" class="pane pane-right" hidden>`」——❌ 不存在**

`index.html` 中没有任何 `thread-panel` 元素。`aside.pane-right`（第 132 行）是一个空的面板骨架（仅用于 channel list / member list / message list 的三栏布局），没有 hidden 属性，也没有线程相关内容。

**「`btn-thread` 按钮 display:none」——❌ 不存在**

grep 结果显示 HTML 中没有 `btn-thread`。唯一线程相关的 UI 元素是 `msg-act-mute`（静音线程按钮，`app.js` 第 669-694 行）。

不过核心论点（线程 UI 视图缺失）**是正确的**：没有线程侧面板、没有线程内消息流、没有线程创建对话框。API 层面的 `thread_subs`/`thread_summary`/`thread_participants` 已全部实现。

> **修正建议**：去掉关于 HTML 元素的页码和 ID 引用，重新表述为「Web 前端完全无线程 UI 视图——API 层已就绪」。

---

### 方向四（媒体面可观测）——⚠️ WHIP/SRT 已有指标

**「WHIP/WHEP 指标：零」——❌ 错误**

`crates/aero-live-whip/src/metrics.rs` **已有** 3 个指标：
- `aero_whip_rtp_packets_received_total`（counter）
- `aero_whip_depacketize_failures_total`（counter）
- `aero_whip_active_sessions`（gauge）

测试中也验证了它们稳定存在。

**「SRT 指标：仅 metrics.rs 有部分计数器」——✅ 基本准确**

`aero-live-srt/src/metrics.rs` 有 4 个指标（active_sessions, packets_received, packets_lost, bytes_received）。相对于你文档中建议的「连接质量（RTT/丢包率/带宽估计）」而言确实不全。

**「RTMP 指标：零」——✅ 正确**。`aero-live-rtmp` 没有 `metrics.rs`，也没有任何指标调用。

**「SFU 指标：零」——✅ 正确**。`forward/mod.rs` 没有指标。

**「Call Bridge 指标：零」——✅ 正确**。

**「HLS 指标：零」——✅ 正确**。唯一 `ts.rs` 中的 "counter" 指的是 MPEG-TS 的 continuity counter（4-bit），不是 Prometheus counter。

> **修正建议**：将所有「零指标」拆分为三档——✅ 零（RTMP/SFU/Bridge/HLS）、⚠️ 有基础指标（SRT 4 个/WHIP 3 个）、❌ 缺失质量指标（RTT/jitter/packet loss ratio）。

---

### 方向五（流摄入韧性）——✅ 基本准确

所有代码核查确认了你提出的断线重连、failover、流健康监控缺口：

- RTMP：无 `Reconnecting` 状态、无自动重连
- WHIP：无 ICE restart 处理（`whip_post` 不处理 `PATCH` ICE restart）
- SRT：`protocol.rs` 无 `HSREQ/HSRSP` 连接迁移
- `LiveService::handle_end` 直接设为 `Ended`

这是文档中**最准确的**方向——所有代码主张都站得住。

---

## 总体评估

| 维度 | 评级 | 说明 |
|------|------|------|
| **方向选择** | ⭐⭐⭐⭐⭐ | 5 个方向确实均无既有分析**系统性**覆盖 |
| **代码准确性** | ⭐⭐⭐ | 方向一有 3 处严重事实错误（后端子系统和前端问题混淆） |
| **前端细节准确性** | ⭐⭐ | `index.html` 元素引用全部不准确（btn-thread / thread-panel 不存在） |
| **媒体面准确性** | ⭐⭐⭐ | WHIP/SRT 已有指标被忽略 |
| **产品推理** | ⭐⭐⭐⭐⭐ | 产品层面的影响分析有深度，边界情况覆盖周全 |
| **工作量估算** | ⭐⭐⭐⭐ | 整体合理，但方向一过高（~11 天→实际 ~5 天） |

总的来说是一份有价值的分析，但代码准确度需要在归档前修正。要我帮你修正成一份准确的版本保存到 `docs/requirements/` 下吗？
