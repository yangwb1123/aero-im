## 需求评估（资深经验处方）
需求: 实现消息撤回与发送确认：发送方可撤回自己的消息并查看送达确认，生产环境，Rust 后端
画像: backend [unknown/unknown] | 匹配档 production → 处方档 demo（规模 S）| 风险 low
完整性: 2/8 — 缺失: user_role, main_flow, permission, error_path, acceptance, tech_stack
工作流: L0_direct（分 0）— 单任务直接修改 + 快速验证门禁（无需流水线）
产品化: L0_local_feature
  克制: L0 小工具需求：禁止产品化结构，只实现需求本身（克制原则）
处方（最小必要规则集，共 1 条）:
- [必选] architecture (demo): 模块组织（业务能力优先）、依赖方向、数据所有权、组合优于继承
    backend-specs/architecture.md
刻意未选用（克制原则）:
- evolution: 需求评估为 demo 档（规模 S），该规则需要 production 档
- ddd: 需求评估为 demo 档（规模 S），该规则需要 production 档
- complexity-scale: 需求评估为 demo 档（规模 S），该规则需要 production 档
- production-readiness: 需求评估为 demo 档（规模 S），该规则需要 production 档
- architecture-constitution: 需求评估为 demo 档（规模 S），该规则需要 production 档
- system-engineering: 需求评估为 demo 档（规模 S），该规则需要 production 档
- network-engineering: 需求评估为 demo 档（规模 S），该规则需要 production 档
- design-patterns: 需求评估为 demo 档（规模 S），该规则需要 standard 档
- oop-di: 需求评估为 demo 档（规模 S），该规则需要 standard 档
- agent-guardrails: 需求评估为 demo 档（规模 S），该规则需要 standard 档
- testing: 需求评估为 demo 档（规模 S），该规则需要 standard 档
建议补充: 未提及用户角色与权限：按钮可见性/操作范围无法评估
建议补充: 未描述主流程：交互链路与状态机无法设计
建议补充: 未提及权限：审批/删除等敏感操作的可见性无法评估
建议补充: 未提及异常路径：失败恢复与重试策略无法设计
建议补充: 未提及验收标准：完成定义缺失，无法判定交付
建议补充: 未指定技术栈：平台适配（tsx/dart/vue）为假设
