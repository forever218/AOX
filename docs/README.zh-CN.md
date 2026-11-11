# AOX 文档索引

[← README](../README.zh-CN.md) · [English](README.md)

本仓有两种写作：

- **`docs/`** —— 设计记录：架构怎么走到今天、每条缝是什么、什么已定、什么还开着。**英文是源
  语言**，多数文档旁边有一份 `*.zh-CN.md` 副本。
- **`.workbuddy/memory/`** —— 工作笔记：动手**之前**该读的操作层细节（陷阱、当前形态、历史坑）。
  仅中文。

想要全局图景，从**路线图**开始；正准备改代码，从**笔记**开始。

---

## 先看这两份

| 文档 | 内容 |
|---|---|
| [`USAGE.zh-CN.md`](USAGE.zh-CN.md) | **使用手册** —— 安装、模型、全部命令、GUI 各窗口、会话与空间、配置分层、工具、边界、回滚、远程、打包 |
| [`ARCH-ROADMAP.zh-CN.md`](ARCH-ROADMAP.zh-CN.md) | **为什么这样设计** —— 七条设计原则，以及它们推出来的分阶段计划 |

## 1. 设计记录（`docs/`）

| 文档 | 覆盖内容 | 状态 | 中文副本 |
|---|---|---|---|
| [`ARCH-ROADMAP.md`](ARCH-ROADMAP.md) | **路线图** —— 七条设计原则、阶段 1–7，以及每次构建的**体积全史** | 活文档；阶段 1–6 已验收，7a/7b 待验收 | [✓](ARCH-ROADMAP.zh-CN.md) |
| [`SANDBOX.md`](SANDBOX.md) | **围栏线** —— 插件子进程现在实际被挡住了什么、多挡一点要付什么代价（①/②a/②b/③/④ 选项对比） | ① JobObject + ③ 工作区边界已实现、待验收；②a / ②b / ④ 未做 | [✓](SANDBOX.zh-CN.md) |
| [`COMPONENT-CONTRIBUTIONS.md`](COMPONENT-CONTRIBUTIONS.md) | **阶段 6b** —— 插件不再只是「加一个工具」：组件如何贡献 hook、主题、提示词与工具 | 已交付 | [✓](COMPONENT-CONTRIBUTIONS.zh-CN.md) |
| [`I18N.md`](I18N.md) | **国际化** —— 一种语言一个发行版：`t()` / `tf!` 管线、`AOX_LANG`、目录规则 | 已交付（文案阶段完成） | [✓](I18N.zh-CN.md) |
| [`WEB.md`](WEB.md) | **联网工具** —— `web_fetch`：触达范围（`[web] egress`）、正文提取、游标，以及它**不是**网络边界 | M2 已交付；缓存、JS 渲染、`web_search` 均立项后砍掉（D5/D10/D11） | [✓](WEB.zh-CN.md) |
| [`SHELL.md`](SHELL.md) | **命令行工具** —— `shell` / `bash`：方言、写入护栏、上限，以及实测得出「它不是内核边界」的经过 | 2026-10-20 已交付 | — |
| [`ROLLBACK.md`](ROLLBACK.md) | **任务检查点** —— 恢复点、回滚 / 重做契约、保留策略与磁盘结构 | 设计记录；实现见 [`ROLLBACK-IMPLEMENTATION.zh-CN.md`](ROLLBACK-IMPLEMENTATION.zh-CN.md) 与 [`ROLLBACK-GUI.zh-CN.md`](ROLLBACK-GUI.zh-CN.md) | [✓](ROLLBACK.zh-CN.md) |
| [`UI-CUSTOMIZATION.md`](UI-CUSTOMIZATION.md) | **立项册**：UI 缝 —— 元素 id、属性覆盖、具名插槽、沙箱化补丁 | **已立项、延后**（2026-10-03） | [✓](UI-CUSTOMIZATION.zh-CN.md) |
| [`SUBAGENT.md`](SUBAGENT.md) | **立项册**：子代理 —— 自孵化无界面实例、协议、三条隔离轴、授权策略 | **已立项、延后**（2026-10-09） | [✓](SUBAGENT.zh-CN.md) |

图表（自包含 HTML，浏览器直接打开）：

| 文件 | 展示内容 | 语言 |
|---|---|---|
| [`TOOL-PIPELINE.html`](TOOL-PIPELINE.html) | 工具**执行流水线** | 中文 |
| [`TOOL-PIPELINE-TOPO.html`](TOOL-PIPELINE-TOPO.html) | 工具执行**拓扑** | 英文 |

## 2. 路线图各阶段一览

取自 [`ARCH-ROADMAP.md`](ARCH-ROADMAP.md#progress) 的 Progress 表。前六个阶段是**可扩展性**轴；
阶段 7 起是**安全边界**轴。

| 阶段 | 主题 | 状态 |
|---|---|---|
| 1 | 提示词分段（prompt 缝） | ✅ 已验收 2026-10-03 |
| 2 | 外观即数据（theme 缝） | ✅ 已验收 2026-10-03 |
| 3 | 存储与策略缝（store 缝 + policy 缝） | ✅ 已验收 2026-10-03 |
| 4 | 循环钩子（loop 缝，第一形态） | ✅ 已验收 2026-10-03 |
| 5 | 分层组合清单（组合层） | ✅ 已验收 2026-10-03 |
| 6a | 整个循环可替换 | ✅ 已验收 2026-10-03 |
| 6b | 组件贡献 | ✅ 已验收 2026-10-03 |
| — | UI 定制层（原 6c） | ⏸ 已立项、延后 |
| 7a | 插件围栏 · ① JobObject | ⏳ 待验收 |
| 7b | 工作区边界 · ③ 文件能力收回宿主 | ⏳ 待验收 |
| — | 子代理（自孵化无界面实例） | ⏸ 已立项、延后 |

图例：✅ 已验收 · ⏳ 已实现、待验收 · ⏸ 已立项、延后 · 🚧 未开始

## 3. 工作笔记（`.workbuddy/memory/`，中文）

这是操作层 —— 改对应区域之前先读相关那本。

| 笔记 | 何时读 |
|---|---|
| [`MEMORY.md`](../.workbuddy/memory/MEMORY.md) | **永远** —— 提炼后的长期笔记：进度、绝不可破的规则、i18n 约定、改动安全清单 |
| [`SANDBOX.md`](../.workbuddy/memory/SANDBOX.md) | 动沙箱、GUI 浮层，或 `src/sandbox.rs` / `src/workplace.rs` 里的任何东西时 |
| [`SEAMS.md`](../.workbuddy/memory/SEAMS.md) | 动七条缝中的任意一条时 —— 「改缝前必须知道」的细节层 |
| [`TRAPS.md`](../.workbuddy/memory/TRAPS.md) | 撞到怪事时 —— GUI/egui 0.33 陷阱、运行时分发陷阱、**历史文件格式**、token 统计 |
| `YYYY-MM-DD.md` | 追查某事**何时、为何**变化时 —— 只追加的日日志 |

## 4. 约定

- **双语**：`docs/` 每份文件英文为源，`*.zh-CN.md` 副本必须同步。`.workbuddy/memory/` 下的笔记
  仅中文。
- **换行**：改完跑 `tools/fix_newlines.py`（按 `HEAD` 还原 CRLF/LF）。
- **编码**：碰过任何中文文件后扫 U+FFFD —— `grep -rInP '\x{FFFD}' src gui/src docs`。
- **节奏**：路线图 → 确认 → 实施 → 验收 → 下一阶段。架构重构期**不自动 commit**，由用户按阶段
  手动提交。
- **零漂移**：每个阶段都必须让默认配置的行为与之前完全一致，且上一阶段验收后才开下一阶段。
