现在我掌握了完整的代码交叉验证结果。以下是作为架构师的深度审阅：

---

## 代码交叉验证结果

### ✅ 完全准确的断言

| 断言 | 验证结果 |
|------|---------|
| `peer.rs:146` — "no transcode" 注释 | ✅ 精确在 146 行 |
| `HlsWriter` 只有单 `index.m3u8`，无 variant playlist | ✅ 零匹配 playlist/variant/rendition |
| `PasswordPolicy` 只有 `min_len`/`max_len`/`required_classes` | ✅ 代码一致 |
| 密码策略无 HIBP/common-password/expiry/breach | ✅ 零匹配 |
| 迁移 0148 是 shadow table + 需要维护窗口 | ✅ 注释明确说了 `"DELIBERATELY NOT in the auto-applied chain"` |
| `sqlx::migrate!` 无超时 | ✅ 验证 |
| 最新迁移编号 0157 = 157 个迁移 | ✅ 匹配文档 |
| hls.js CDN 引用存在 → ABR 前提成立 | ✅ `web/index.html` 引用 jsdelivr |
| `SfuForwarder::on_rtp` 不存在 "record"/"archive" | ✅ 零匹配 |

### ⚠️ 需要修正或细化的主张

**方向二 API 命名错误**：文档写的是 `SfuMediaSession.on_rtp` 作为录制 hook，但实际代码中方法是 `deliver()`，且调用点为：
```rust
// sfu_media.rs:64
fn deliver(&self, rtp: &InboundRtp) {
    self.forwarder.on_rtp(self.peer.id(), rtp);  // ← SfuForwarder.on_rtp, 不是 SfuMediaSession
    // 录制 hook 应在这里
}
```
架构理解对（RTP 流经此处），但 API 名错了一个层级。

**方向一 Phase A 是伪方案**："纯解码后用软件缩放，不必硬编" — H.264 不解码就不可能降低码率/分辨率。变体输出必须走 ffmpeg 解码 → 缩放 → 重编码，没有捷径。Phase A 应如实要求 ffmpeg 从第一天起。

**方向四低估 S3 presigned URL 复杂度**：
- 手写 SigV4 目前是 **Authorization header** 签名，presigned URL 需要 **query parameter** 签名（`X-Amz-Signature`），是不同规范
- 浏览器 S3 直传需要 **Presigned POST**（不是 PUT），需要指定 policy 条件
- `confirm-upload` 流有 TOCTOU 风险：客户端上传文件 A 但回调声称文件 B 的尺寸/ETag
- `LocalFsBlobStore` 不支持 presigned URL（无意义），需要 trait 方法签名有 fallback

**方向五最大约束没触及**：`sqlx::migrate!` 在每个迁移的**事务内**执行。`CREATE INDEX CONCURRENTLY` **不能在事务块内执行**。这意味着任何涉及 `CONCURRENTLY` 的迁移都无法通过当前迁移框架执行。要么换回 `sqlx::query` 手动执行，要么迁移框架需要重做。

### 🟢 高价值未提及的边界情况

每个方向都遗漏了 1-2 个关键运行时约束：

**方向一转码**：编码器是 CPU/GPU 共享资源，但文档没考虑**推流中断重建时转码管线的状态恢复问题**。如果编码器进程在推流途中崩溃，新转码管线启动时需要对齐关键帧，否则 HLS 变体段边界漂移 → 客户端 ABR 切换出现花屏。

**方向二录制**：多人通话录制的最难问题不是捕获 RTP 包，而是**混音 + 画中画合成**。当前 `deliver()` 一次处理一个参与者的一个 RTP 包，没有跨流时间对齐。SFU 侧最简单的方案是只录发言者音频 + 屏幕共享视频，而不是完整混流。文档应该分清楚 "单轨录制" vs "合成录制"。

**方向四 S3 直传 + upload 安全**：`confirm-upload` 需要 `HEAD` S3 对象来验证大小/ETag，这个请求本身也有网络延迟。更好的模式是 S3 **Event Notification** → SQS → webhook，或者完全去掉 confirm 步骤改为客户端签名时绑定约束（如 `content-length-range`）使上传前就无法作弊。文档没提到 S3 event notification 路径。

**方向五 partition cutover 的量级**：0148 注释说涉及 **7 inbound FK rewrites across 6 child tables**（`messages` 表有 6 个子表引用它的外键）。`RENAME` 操作会阻断这些子表上的所有查询。这就是为什么 runbook 要求维护窗口。文档没量化这个锁的影响范围。

---

这份分析的代码验证扎实度不错（45+ 条代码证据全数可复现），但 5 个方向里有 3 个存在 **API 名/实现难度/架构约束的显著低估**。最有价值的发现其实是方向五——**157 个迁移 + `sqlx::migrate!` 的事务内执行限制 + 0148 的 7 个外键重写**，这个组合在生产运维层面的真实风险远超文档估算的 3-4 周工期。

要我把这些修正点写入文件（以 `2026-07-11-code-verified-five-directions.architect-review.md` 形式归档），还是你更想深入展开某个方向的修正方案？
