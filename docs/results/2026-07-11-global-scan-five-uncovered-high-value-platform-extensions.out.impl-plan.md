完整的 Tech Lead 实施计划已写入 `docs/analysis/2026-07-12-tech-lead-implementation-plan-five-strategic-directions-from-review.md`（609 行，~56KB）。

## 文档结构概览

### §1 任务分解
将 5 个方向拆解为 **56 个可执行任务**（每个 2-8h），按 Phase 0/A/B/C 编排：

| 方向 | 任务数 | 总工时 | 并行度 |
|------|--------|--------|--------|
| 一·AI 会议 | 8 | ~28h | 🟡 中（per-peer 路由是关键依赖） |
| 二·语音/视频 | 8 | ~29h | 🟡 中（UX 依赖实体激活） |
| 三·直播货币化 | 8 | ~34h | 🔴 低（金融级代码需严格审核） |
| 四·多协议网关 | 9 | ~37h | 🟢 高（sidecar 完全独立） |
| 五·信任安全 | 9 | ~37h | 🟢 高（Phase A 非侵入式） |
| **总计** | **42** | **~165h** | |

### §2 执行顺序
- **Group A（Week 1-2，11 个任务完全并行）**: 设备指纹列、Block::Video 验证、bot 崩溃恢复基础、email_aliases 表、virtual_coins 表、inbound webhook 等所有非侵入式基础设施
- **Group B（Week 3-4）**: 核心功能——per-peer ASR、录音 UX、Stripe 购买、IMAP
- **Group C（Week 4-6）**: 纪要×tasks 交叉引用、Safari 降级管线、礼物消费、Sybil 检测
- **Group D（Week 7-10）**: Stripe Connect、Matrix/Discord bridge、CSAM 哈希匹配、内部威胁检测

### §3 技术风险（10 项）
Top 风险：
- **R1**: SFU 层不暴露帧级 ParticipantId → 替代方案：Whisper 自带 diarize 参数 + SSRC 映射表
- **R3**: Stripe Connect 审核周期不可控 → 工程与审批并行，Phase A 先用标准 Stripe Checkout
- **R5**: CSAM pHash 误报 → 严格的三级置信度策略（SHA256 精确→自动，pHash 模糊→人工，报告前人工确认）

### §4 资源评估
- 最小团队: 3 后端 + 1 前端 + 1 TL = **5 人**
- 外部依赖: 法务 0.5FTE（CSAM 报告 + ToS + Stripe 合规）

### §5 质量保证
- 每个模块的单元测试覆盖率目标（90-95%）
- 并发扣款的 50-tokio-task 压力测试
- 代码审查四点专项：金融级正确性 / Sidecar 架构 / 安全敏感 / AI 管线

### §6 实施时间表
- **Week 3 演示**: 会议 bot ASR + 语音波形 UX + Stripe 购买 + Email 收发
- **Week 6 演示**: 创作者提现 + Matrix桥 + CSAM 检测 + 安全仪表盘
- **Week 7-10**: Stripe Connect 审核窗口期（工程与审批并行）

### 附加价值
- **附录 B**: 合规/工程双时间轴——明确标注 Stripe Connect 审核、ToS 更新、CSAM 法务审查的并行路径
- **跨方向可观测性指标**: 5 个方向各有关键指标和告警阈值
- **明确不做清单（Explicitly Out of Scope）**: 11 项，防止 scope creep
