"""Built-in bilingual keyword registry for the deterministic classifier."""

from __future__ import annotations


# Built-in defaults keep classification available without YAML/PyYAML.
_SYSTEM_TYPE_TERMS = {
    "state-machine": ["状态机", "状态流转", "状态迁移", "审批流", "workflow",
                      "状态变更", "审核", "审批", "生命周期"],
    "event-driven": ["事件", "消息队列", "事件驱动", "kafka", "mq", "pub/sub",
                     "event", "webhook", "通知"],
    "realtime": ["实时", "websocket", "推送", "监控大屏", "告警", "alert",
                 "realtime", "live", "流式"],
    "search": ["搜索", "检索", "推荐", "查询优化", "索引", "search",
               "autocomplete", "模糊匹配"],
    "optimization": ["优化", "排程", "调度", "路径规划", "资源分配",
                     "optimize", "scheduling", "成本最小"],
    "knowledge": ["知识库", "文档检索", "问答", "rag", "知识图谱", "faq",
                  "知识管理", "chat"],
    "batch": ["批量", "定时任务", "批处理", "cron", "导入导出", "etl",
              "batch", "job"],
    "adaptive": ["自适应", "智能推荐", "预测", "风险评估", "机器学习",
                 "ai", "模型", "agent", "智能"],
    "collaboration": ["协作", "多人", "团队", "权限", "角色", "组织",
                      "collaboration", "分配"],
    "deterministic": ["crud", "增删改查", "表单", "台账", "记录", "报表"],
}

_DEFAULT_KEYWORDS: dict = {
    "task_types": {
        "frontend_ui": [
            "page", "ui", "layout", "component", "spacing", "widget", "screen",
            "tsx", "dart", "flutter", "react", "vue", "css", "styled",
            "admin", "页面", "前端", "界面", "后台", "管理后台", "布局", "组件",
            "间距", "按钮", "表单", "弹窗", "卡片", "导航", "侧边栏", "表格", "分页",
            "登录页", "列表页", "详情页", "设计稿", "视觉",
        ],
        "backend": [
            "api", "server", "endpoint", "database", "schema", "service",
            "后端", "接口", "服务端", "数据库", "表结构",
            "微服务", "分布式", "领域模型", "实体", "聚合", "事务",
            "消息队列", "对账", "库存", "工作流",
            "domain", "entity", "workflow", "transaction",
        ],
        "data": ["sql", "etl", "数据管道", "报表", "指标", "数据仓库"],
        "code_engineering": [
            "重构", "维护", "修复", "优化", "清理", "重构代码", "技术债",
            "refactor", "maintain", "fix", "optimize", "cleanup", "refactoring",
        ],
        "docs": ["documentation", "spec", "readme", "文档", "需求", "规范", "说明", "手册"],
        "devops": ["ci/cd", "deploy", "docker", "kubernetes", "部署", "运维", "发布", "流水线"],
        "analysis": ["review", "audit", "分析", "审查", "评审", "评估", "调研"],
    },
    "platforms": {
        "tsx": ["tsx", "react", "typescript", "antd", "前端组件"],
        "dart": ["dart", "flutter", "widget", "sizedbox", "material"],
        "vue": ["vue", "nuxt", "template"],
        "rn": ["react native", "rn"],
    },
    "profiles": {
        "erp": ["erp", "mes", "排程", "库存", "采购单", "工单", "单据"],
        "cms": ["cms", "内容管理", "文章", "审核", "发布"],
        "oa": ["oa", "审批", "待办", "流程", "通知公告"],
        "dashboard": ["dashboard", "大屏", "图表", "数据可视化", "看板"],
        "immersive": ["特效", "3d", "沉浸", "动画", "滚动叙事", "粒子"],
        "marketing": ["官网", "落地页", "营销", "转化", "landing"],
        "mobile": ["移动端", "app", "mobile"],
    },
    "system_types": _SYSTEM_TYPE_TERMS,
}
