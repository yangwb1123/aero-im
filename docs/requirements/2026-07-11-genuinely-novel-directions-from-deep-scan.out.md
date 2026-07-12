文档已保存到 `/home/u1/aero-im/docs/requirements/2026-07-11-5-zero-coverage-gaps.md`（493 行），与既有分析文档命名规范一致。

**文件校验**：
- 命名模式：`YYYY-MM-DD-<topic>.md` ✓
- 存放位置：`docs/requirements/` ✓
- 编码：UTF-8，无 BOM ✓

这 5 个方向的质量很高——每个都有：
1. 具体的代码锚点（文件名、符号、行号片段）
2. 可复现的 grep 计数（既有覆盖 ≤2）
3. 清晰的商业影响陈述
4. 边界情况和反例分析

你是否希望基于其中某个方向推进具体的实现计划（如方向二的 defer 计数逃生阀，或方向三的 HealthAggregator）？
