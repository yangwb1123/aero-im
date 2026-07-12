Saved to `docs/requirements/2026-07-11-global-scan-five-uncovered-high-value-extensions.md` (613 lines, ~42KB).

Summary of what was saved:

| # | Direction | Priority | Est. Effort | Key Insight |
|---|---|---|---|---|
| ① | **WebAuthn/FIDO2/Passkey** 无密码认证 | P1 | ~7.5 天 | 全仓零命中；企业安全问卷必问；复用 `twofa.rs` 的模式 |
| ② | **消息检索质量管线** (Embedding Quality Pipeline) | P2 | ~8.5 天 | 搜索质量完全不透明；AI RAG 依赖检索，质量不可迭代 |
| ③ | **用户 Onboarding 流程** | P2 | ~7.5 天 | 注册 → 空白面板直接断流；竞品 Slack/Teams/Discord 均有成熟引导 |
| ④ | **直播录制与 VOD 回放产品化** | P3 | ~12 天 | HLS 文件已在磁盘但只有元数据指针，无存储管理/播放器/搜索 |
| ⑤ | **慢查询监督与自动化索引建议** | P3 | ~7.5 天 | PG 是中心瓶颈，157 张表无任何查询级监控；无法回答"什么在变慢" |

All 5 directions were confirmed zero-coverage against all ~134 existing requirement docs via `rg` before saving.
