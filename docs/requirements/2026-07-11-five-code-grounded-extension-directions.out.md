感谢分享这份分析。我交叉比对了 `docs/requirements/` 下全部既有文档，有以下事实核查结果：

---

## 新颖性评估：5 个方向中 4 个已被系统性覆盖

对照 grep 结果，文档声称「未系统性覆盖」与既有分析集合的实际情况有显著偏差：

| 方向 | 文档自称 | 实际覆盖状态 | 关键已有分析 |
|------|---------|-------------|-------------|
| **① WHIP→WHEP** | 未系统性覆盖 | ⚠️ **部分新颖**——桥接缺失已知，但 `NalSource` 频道扇出方案是新增细节 | `prod-scale-perspective.md` 提到 sticky routing / WHEP 跨实例拉流；`five-code-verified-product-gaps.md` 提到浏览器端未接线 |
| **② SFU 未 Production 接线** | 未系统性覆盖 | ❌ **深度覆盖**——至少 5+ 份分析使用相同代码证据（`#[cfg(test)]` / `bind+run` 零调用方） | `architectural-blindspots-after-comprehensive-scan.md` §4；`five-code-grounded-extension-directions.md` 方向二；`core-expansion-analysis.md`；`edge-cases-and-extension-directions.md` |
| **③ 客户端帧 handler 缺失** | 未系统性覆盖 | ❌ **深度覆盖**——`MessageSeen` 和 `Interaction` 缺失已在 3 份分析中完全记录，含具体帧代码映射 | `five-critical-gaps-final-scan.md` 方向三；`code-verified-gaps.md` §MessageSeen；`production-gaps.md` |
| **④ API 无版本化** | 未系统性覆盖 | ❌ **深度覆盖**——`prod-scale-perspective.md` 方向二为完整 API 治理分析（含 WS 版本协商、枚举兜底、Sunset header、OpenAPI 自动生成） | 同一方向在 `five-uncovered-high-value-directions.md`、`producer-edge-config-gaps.md`、`strategic-product-directions.md` 等均有涉及 |
| **⑤ HLS 无访问控制/CDN** | 未系统性覆盖 | ❌ **深度覆盖**——安全缺口在 `code-verified-production-failure-modes.md` 方向二；防盗链在 `strategic-product-directions.md`；CDN 架构在 `prod-scale-perspective.md` | 三者独立存在且附带代码证据（`ServeDir::new(&hls_dir)` L111 为共有锚点） |

---

## 结论

**方向②③④⑤** 的新颖性声明与既有 110+ 份分析集合不符——这些方向之前已被系统性覆盖，且部分（方向②方向④）已有更详细的修复方案（含 mermaid 流程图、阶段性工作量估算、边界情况清单）。

**方向①（WHIP→WHEP 桥）** 的 `NalSource` 频道扇出机制是目前较新颖的切入点——此前分析仅指出 WHIP/WHEP 联调缺失，未细化到 NAL 单元中继层的设计方案。但即使是方向①，`WhepSession::run` 无调用方这一事实也已在 `architectural-blindspots-after-comprehensive-scan.md` 中标注。

如需要，我可以：
1. 提取既有分析中**真正零覆盖**的方向（第 1 轮～第 33 轮扫描后仍存留的条目）
2. 或直接跳到方向①的实现方案详细设计
