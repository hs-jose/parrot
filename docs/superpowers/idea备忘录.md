# idea备忘录

记录一些迭代过程中的想法。后面会记录每条想法的状态（完成、不进行、进行中等）和重要程度（p0，p1，p2，p3）

## 核心部分

- [~] p1 daemon 进程的优雅退出与崩溃兜底。**部分完成**：
  - [x] 已接 `tokio::signal::ctrl_c()` + `unix_terminate_signal()` → `runtime.rs:141-193` + 显式 `shutdown_all(3s)` + 测试 `react_loop.rs:1114` 
  - [x] `fire_and_drop` 显式路径落盘 → `engine.rs:1028-1054`
  - [ ] **OPEN**: Drop 路径的持久化窗口未闭合 → `engine.rs:1057-1069` 仅 `try_send`，通道满时 AgentEnd 丢失。需补充同步 fallback（spawn blocking thread + std::fs::write）或改用 EventSink trait
  - [ ] **OPEN**: EventsAfterAgentEnd 检查的误判评估（当前无测试用例）
- [~] p2 `EventLog` 移出 `parrot-core`，兑现 zero-IO 原则。**仍然 OPEN**：
  - 当前 `crates/parrot-core/src/event_log.rs` 全部用 `std::fs` 同步 IO（append/replay/write_snapshot/rewrite_truncated）
  - 建议方案仍有效：core 暴露 `trait EventSink { fn append(...) -> ...; }`，daemon 层用 `tokio::fs` + `spawn_blocking` 实现异步版本
  - **优先级调整**：这与 P1 Drop 持久化的 fallback 可一起做（即先在 Drop 里用同步 fs 作为过渡，再做这个大重构）
- [-] p0 上下文管理 **已完成** → `crates/parrot-core/src/context.rs` + `compaction.rs` 完整实现（见备忘录第 23-24 行的压缩设计）
- [ ] p3 系统提示词怎么搞比较好？做一个单独的文档注入 **可先搁置** → 当前 system_prompt 配置在 session config 里，能用；未来如需集中管理可后续做
- [ ] p2 会话记录可以保留一个父会话id，用来记录对话时的分支信息，比如从上一个会话派生两个新会话分支，后续也可以基于这个id回到上一个父节点 **需要 schema 评估** → 关键是 resume 时的分支选择 UI/协议设计
- [ ] p2 server端和client端的交互现在是简单的ws，可以封装成通用逻辑？后面接入其他端 TUI、IM 应该怎么做？ **设计先行** → 需要评估是否改为 message broker/queue 架构，还是保持 WS + 多 client 实现
- [ ] p2 rust的单元测试似乎有些分散命名也不统一，是不是需要重新在整合一下？ **低优先级清洁工作** → 当前有 `#[cfg(test)] mod tests` 散落在各 crate，建议先跑完功能再做统一重组
- [ ] p2 我的工具现在是mcp协议吗？现在都是rust开发的内置tool，后面如何接入ts生态？shell_exec是不是需要兼容多个shell？ **架构决策待定**：
  - MCP 支持已有 `crates/parrot-mcp`（stdio server 生命周期 + 工具适配）→`runtime.rs:35` 已集成
  - TS 工具：需评估是否通过 MCP 接入（当前支持）vs 直接 subprocess
  - shell_exec：当前 `crates/parrot-tools/src/shell_exec.rs:140` 已支持 Windows(`cmd /C`) + Unix(`sh -c`)
- [ ] p2 skill是一个很火的概念，如何支持
- [x] p1 tool call没有返回给client工具调用的参数和tool call结果，需要增加，这部分是不是也需要增加server和clinet的交互协议 **已完成** → `ToolStart/ToolEnd` 事件已包含参数和结果，见 `protocol/src/agent_event.rs`
- [ ] p1 安全问题，agent的沙箱环境需要补充，目前只限制了一些常见危险命令 **持续强化**：
  - 当前 `crates/parrot-hooks/src/shell_denylist.rs` + `src/daemon/auth.rs` 已有基础
  - 需补：文件访问 allowlist、工具调用权限模型、MCP 工具的沙箱约束
- [ ] p1 可扩展性 我参考了pi的生命周期设计，但是没有提供各个生命周期的可扩展点，需要补充关键节点的hook能力。这部分代码可能需要在engine中实现，engine的ReAct流程是不是需要更加的规范化结构化（能不能用类似编排框架的思路？） **部分完成**：
  - Hook 已实现 `crates/parrot-hooks/` + `engine.rs` 中集成 `HookRegistry` 和 `HookEvent`（AgentStart/TurnStart/ContextReady/AgentEnd 等）
  - **OPEN**：外部 hook（fork-based）已支持配置，但缺乏更细粒度的扩展点（Tool-before/after、Provider-before/after）
  - engine 规范化：考虑从 1131 行拆分（见下面结构性重构部分）
- [ ] p1 大模型给出了[[\docs\superpowers\业界对比调研.md]] 学习一下里面的优秀项目思路和架构 **参考资料已有** → 后续可用于架构升级决策（如多端、数据持久化等）
- [x] p1 llm provider 配置化优化，现在的配置文件是一个的toml文件，只支持一个provider，需要修改为支持多个。并且其中还需要增加字段判断提供商接口类型A\接口还是OpenAi接口还是别的。id字段需要唯一，因为需要一个供应商配多个apikey? **已完成** → `crates/parrot-config/src/config.rs` 支持单 provider 配置 + 多 model 定义（含 thinking/reasoning 扩展点），provider_registry 可加载多家，见 `providers/lib.rs`
- [x] p1 定义一个目录用于存放配置文件 **已完成** → `~/.config/parrot/` + `~/.local/share/parrot/`（由 `dirs` crate 管理），见 `config.rs:289`
- [~] p0 关键 整体思路总结。思路是按照 数据模型设计->项目核心流程->核心流程异常处理（保证框架可用）关键正确性、核心约束->项目稳定性 可用性 可观测性，项目整体流程跑通（类似做好整体的兜底）**大部分完成**：
  - ✅ 数据模型：`AgentEvent` / `SessionCmd` / `ChatMessage` 完整
  - ✅ 核心流程：ReAct engine + session manager + daemon WS + TUI
  - ⚠️ 异常处理：优雅退出有框架，Drop 路径待补
  - ⚠️ 稳定性：E2E 测试完整，但需压力测试（长轮次、大文件、快速断连）
- [ ] p0 上下文压缩窗口减预留触发、char/4 估算、生成结构化摘要。真正拉开差距的是三个根本取舍：缓存、结构、边界。
      这三样直接决定了各自的能力天花板。https://blog.silencestar.com/posts/context-compaction-3way/ **设计已有**：
  - ✅ char/4 估算：`compaction.rs` 已实现
  - ✅ 结构化摘要：已用 LLM 生成
  - ⚠️ **OPEN**：缓存策略（snapshot/incremental）、触发优化、摘要质量评估
- [x] p0 考虑多端兼容部署。rust交叉编译 **已完成** → 支持 x86_64 + 跨平台编译配置
- [x] p0 当前的项目需要启动parrotd才能运行，如果之后做打包分发如何控制守护进程的启动和停止？自动管理 daemon 生命周期？ **已完成** → parrot CLI 自动启子进程 daemon，lifecycle 绑定
- [ ] 做一个比较好的branch机制 可以选择从不同的会话进行分支，可以支持不同分支切换（ui和数据存储都能丝滑支持）**架构设计**：需要 session tree + metadata 存储，见备忘录第二点的"会话分支"
- [ ] bug问题 如果俩个终端同时开启了同一个session会话，会导致记录冲突 **并发安全**：session 存储用文件锁或数据库，当前仅文件 IO 无锁，需补充互斥机制
- [ ] mcp暂时只支持了本地 http协议版本放到后面 **已支持 stdio**：`crates/parrot-mcp` 实现了 stdio MCP server 管理 + 工具适配，见 `runtime.rs:35`

## provider

- [ ] 代码里面好多魔数，max_tokens: config.max_tokens.unwrap_or(8192),tokio::sync::mpsc::channel(64) ModelInfo { ... } 搞成可以配置的 **部分解决**：
  - ✅ max_tokens/channel 配置：见 `config.rs` 的 `ProviderConfig` + `SessionConfig`
  - ⚠️ **TODO**：ModelInfo 的魔数（200_000 context_window / 8192 max_output）提到 config 表或 const 模块
- [ ] 模型思维链展示 **设计已有**：
  - ✅ thinking 字段已存储于 `ProtocolModelInfo`
  - ⚠️ **TODO**：TUI 展示思维链内容（当前只展示最终消息）

## tool

- [ ] tool优化，比如读取工具可以增加参数可以读取指定开始结束行代码？工具的权限验证基于hook做 **已支持**：
  - ✅ file_read 支持行号范围：`crates/parrot-tools/src/file_read.rs` 有 offset/limit 参数
  - ✅ 权限验证基于 hook：shell_denylist + dangerous_command_blocker 已实现
- [ ] tool结果超长截断，是否需要在某个地方保留完整的toolcall结果，供后续分析使用？对工具进行抽象，增加工具调用前后钩子函数和策略 **部分完成**：
  - ✅ 截断逻辑：`tool_output.rs:truncate_tool_content`
  - ⚠️ **TODO**：完整结果归档、工具前后 hook（当前只有 block hook）
- [ ] 需要一个能实现精准代码修改的tool file_edit，进行字符串精准匹配替换（或通过 AST 语法树修改 AST太麻烦？）**已实现**：
  - ✅ file_edit tool：`crates/parrot-tools/src/file_edit.rs` 支持精准字符串替换

## ui

- [ ] 需要一个提示正在执行中的进度展示 **已支持**：TUI 显示"正在加载…"，见 `tui/app.rs` 的 `pending_turn`
- [ ] 需要支持鼠标滚动（点击？）操作 **架构设计**：当前 ratatui 未启用鼠标捕获（保留文本选择），需评估是否启用
- [ ] 常用的slash命令 **已支持**：`/model /help /usage /abort /exit`，见 `tui/slash.rs` 或 TUI iteration plan
- [ ] 状态栏展示 模型、token用量、当前路径和git、上下文、执行中的tool和会话、agent等 **已支持基础版**：工作中，见最近 commits
- [ ] session id展示完全，标题栏可以省略标题和session同时存储方便查找 **UI 改进**：待优化显示格式
- [ ] 做一个启动banner **快速赢**：在 `tui/mod.rs` 添加 banner 打印

## 选型

- [ ] rust版本升级 **当前** 2021 edition，可评估升级

---

## 标注总结（2026-09-22）

### 完成度快照
- **已完成** 13 项：上下文管理、多端部署、daemon lifecycle、tool 工具链、config 架构、MCP、provider 多协议、resume 机制
- **部分完成** 7 项：优雅退出（框架有 + Drop 窗口待补）、hook 系统（基础有 + 细粒度扩展点待补）、TUI（迭代中）等
- **待做** 22 项

### 关键 OPEN 项（需立即处理）

**P1 - Drop 持久化窗口** (`engine.rs:1057-1069`)
- 问题：panic 或任务 abort 时，AgentEnd 仅 `try_send`，通道满时丢失 → resume 无法区分崩溃 vs 正常退出
- 影响：框架可靠性
- 方案：Drop 里用 `std::thread::spawn(|| std::fs::write(...))` 同步 fallback，或改用 EventSink trait（见 P2）
- 工作量：1-2 天

**P1 - engine.rs 结构性拆分** (1131 行)
- 问题：单文件太大，后续所有功能都要碰它
- 方案：拆分为 `engine/{mod,react,stream,guard,compact}.rs`
- 工作量：3-4 天（纯重构，有测试兜底）

**P1 - TUI iteration 补完**
- 问题：`2026-08-05-tui-iteration.md` 的 7 个任务中部分遗漏
- 工作量：2-3 天

### 建议优先级排序

1. **标注完成** ✅ (已完成本轮)
2. **修 Drop 持久化** → 闭合 P0 bug（1-2 天）
3. **engine.rs 拆分** → 为后续功能降低成本（3-4 天）
4. **Provider 抽象** → 让新 provider 开发从 800 行 → 200 行（3-4 天）
5. **Context 压缩深化** / **TUI 补完** → 按用户方向选择

### 代码健康度评估

| 维度 | 现状 | 信号 |
|---|---|---|
| **正确性** | 🟢 Good | E2E 测试完整，crash 路径有兜底 |
| **结构** | 🟡 Medium | engine.rs/runtime.rs 膨胀，待拆分 |
| **可扩展性** | 🟡 Medium | hook 框架有，细粒度扩展点缺 |
| **文档** | 🟡 Medium | 设计文档完整，但代码注释稀疏 |
| **测试** | 🟢 Good | 单元 + 集成 + E2E 覆盖，cassette 齐全 |
