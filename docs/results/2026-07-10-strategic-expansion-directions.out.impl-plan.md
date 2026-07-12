文档已写入 `docs/analysis/2026-07-12-tech-lead-implementation-plan-five-expansion-directions.md`（31KB）。下面是摘要：

---

## 分析报告结构

### §1 任务分解 — 5 个方向 × 3 Phase，共 112 小时

| 方向 | Phase A (快赢) | Phase B (平台) | Phase C (完整) | 合计 |
|------|:---:|:---:|:---:|:---:|
| 一·Web Push 浏览器推送 | 10h | 6h | — | **~16h** |
| 二·Bot/App 生态平台（渐进路径） | 13h | 13h | — | **~26h** |
| 三·Meeting Scheduler | 11h | 5h | 9h | **~25h** |
| 四·前端模块化与 UI 补齐 | 6h | 19h | — | **~25h** |
| 五·字幕→自动章节管线 | 10h | 10h | — | **~20h** |
| **总计** | **50h** | **53h** | **9h** | **~112h** |

每个任务 1-4 小时，标注了涉及文件、前置依赖、验收标准。

### §2 执行顺序 — Mermaid 依赖图 + 并行任务组

- **Group A（10 个任务全并行）**：W1-001~004, B2-001~002+005, M3-001, F4-001, T5-001~002
- **Group B（依赖 A 的后处理）**：6 个任务，约 Day 3-5
- **Group C（Phase 2 全并行）**：18 个任务
- **Group D（Phase 3 串行尾部）**：3 个 Meeting 尾端任务

### §3 技术风险 — 9 个已识别风险 + 缓解策略

关键风险：
- **R1** (高): `web-push` crate 兼容性 — 备选手写 HTTP/2 VAPID
- **R4** (高): `context.js` 全局状态交叉修改 — 先加 `subscribe` 模式 + 访问日志
- **R5** (高): Topic shift 准确率不足 — 多策略 fallback + confidence 标注

### §4 资源评估 — 4-6 人团队

- 最小团队：2 后端 + 1 前端 + 1 TL = **4 人**
- 关键里程碑：M1 (Day 3) → M2 (Day 9) → M3 (Week 5) → M4 (Week 8)

### §5 质量保证

- 单元覆盖率目标：85-95%/模块
- 集成测试 3 层 CI 策略（Layer 1 commit / Layer 2 PR / Layer 3 manual）
- 代码审查清单：AGENTS.md 合规、WS 帧安全、AI 管线降级、幂等性
- 性能基准门禁：p99 < 200ms

### §6 实施时间表 — 9 周甘特图

```
Week 1-2: Phase 1 快赢（Web Push 弹窗 + Slash Command + Meeting CRUD + context.js 拆分 + Topic shift 单测）
Week 3-5: Phase 2 平台（Bot UI + Meeting提醒 + Canvas/Tasks/Approvals UI + 章节端到端）
Week 6-8: Phase 3 完整（周期性会议 + iCal + 等待室）
Week 9:   发布准备（E2E + 基准 + 安全审计 + 文档）
```

核心设计决策：
- **方向二**采纳你的建议，先做 Slash Command 扩展（B2-001~005，3.5 人天）再搞 OAuth（V2）
- **方向五**将字幕→章节作为独立 AI 管线增量，不依赖活动实体模型——`call_recap.rs` + `vod_chapters.rs` 之间只缺一个编排 worker
- **方向四**的 `context.js` 拆分从 `subscribe('msg:...')` 模式规范化开始，新模块禁止直接引用全局状态
