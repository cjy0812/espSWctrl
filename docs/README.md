# 文档地图

本项目的文档按**职责**划分，每份只装一类内容，不重叠。新增文档前先看这张表，确认没有更合适的归属。

| 文件 | 职责 | 明确不装什么 | 谁该读 |
|---|---|---|---|
| [`../AGENTS.md`](../AGENTS.md) | **跨工具通用规则**：如何在这个仓库工作 —— 构建入口、本机踩坑、安全线、NVS 纪律 | 进度台账、阶段编号 | 所有 coding agent（Claude Code / Codex / Cursor / Copilot） |
| [`../CLAUDE.md`](../CLAUDE.md) | Claude Code 入口：`@AGENTS.md` + 调度规则与验收纪律 | 构建踩坑（已在 AGENTS.md，不重复） | Claude Code |
| [`usage.md`](usage.md) | **使用手册**：控制台命令、MQTT topic 与 payload、典型操作、排障、配置项默认值 | 设计论证、硬件接线 | 要用这台设备的人（包括三个月后的自己） |
| [`roadmap.md`](roadmap.md) | **唯一**的进度台账与阶段编号来源 | 设计论证 | 想知道「现在做到哪、下一步做什么」的人 |
| [`architecture.md`](architecture.md) | 软件设计与安全论证：总体架构、TLS 方案、Topic/鉴权/防重放、失联与故障处理 | 阶段划分（指向 roadmap）、硬件接线 | 想理解「为什么这样设计」的人 |
| [`hardware.md`](hardware.md) | 硬件依据：817D 光耦电路、电阻计算、引脚选择、接线步骤、供电接地 | 软件设计 | 动手接线的人 |
| [`mqtt-port.md`](mqtt-port.md) | MQTT 从 C 版移植到 Rust 的施工图（行为清单、API 确认、线程模型、实施阶段） | —— | 做 MQTT 实现的人 |
| [`../scripts/check-portability.py`](../scripts/check-portability.py) | **可执行门禁**：路径中立（P1）、`.cmd` 编码（P2）、`.gitattributes`（P3）、toolchain 组件（P4）。纯标准库，Windows / WSL 通用 | 业务逻辑、构建逻辑（那属于 cargo-build.cmd） | 改完构建脚本或新增平台文件的人 |

## 阅读顺序建议

- **第一次接触本项目**：`../README.md` → `usage.md` §0 → `hardware.md`
- **要用这台设备**：`usage.md` 全文（配凭据、发指令、排障都在这里）
- **要动手改代码**：`../AGENTS.md`（尤其安全线）→ `roadmap.md` 当前焦点 → 对应模块的 `architecture.md` 章节
- **要接线硬件**：`hardware.md` 全文，按 §5.10 的顺序做，不要跳步
- **要改 MQTT 实现**：`mqtt-port.md` 全文 → `roadmap.md` Phase 5

## 生命周期

- `mqtt-port.md` 是**临时施工图**：MQTT 实现完成并验证后删除，其长期有效的结论（topic 结构、防重放设计）应已沉淀在 `architecture.md` §4。
- `roadmap.md` 的勾选状态需要**随代码改动同步更新**，行尾追加 `（YYYY-MM-DD 完成，证据 文件:行号）`。